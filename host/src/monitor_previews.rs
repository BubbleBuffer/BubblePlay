//! Low-resolution snapshots of the monitors that are not being streamed (`monitor-previews-v1`).
//!
//! The pure parts live here: the downscaled image type, the significant-change check, the
//! per-session sender state (rate, byte budget, link gate, counters), the wire message and the
//! JPEG encode. Taking the pictures is `monitor_preview_capture.rs`; sending is the host main loop.
//! Wire format: `docs/architecture/monitor-previews.md`.

use anyhow::{ensure, Context, Result};
use base64::Engine;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Capability string, advertised by both sides in the ctl hello.
pub const CAPABILITY: &str = "monitor-previews-v1";
/// Wire version carried in every message.
pub const WIRE_VERSION: u32 = 1;
/// Message `type` host to client.
pub const MESSAGE_TYPE: &str = "monitor-preview";

/// Snapshot height. The width follows the monitor's aspect ratio, at most `MAX_WIDTH`.
pub const PREVIEW_HEIGHT: u32 = 360;
pub const MAX_WIDTH: u32 = 960;
pub const MIN_WIDTH: u32 = 64;
/// JPEG quality: a 360p desktop picture lands around 20-40 KB, and it is scaled up on screen.
pub const JPEG_QUALITY: u8 = 60;
/// At most one snapshot per monitor per interval.
pub const MIN_INTERVAL: Duration = Duration::from_secs(1);
/// The picture is re-taken for the change check at the same pace.
pub const CHECK_INTERVAL: Duration = MIN_INTERVAL;

/// Change-check grid: cells of `CELL` pixels on the downscaled image.
pub const CELL: u32 = 16;
/// A cell changed when its mean absolute luma difference reaches this (0..255).
pub const CELL_MEAN_DELTA: u32 = 10;
/// The picture changed significantly when at least this share of cells changed
/// (per mille) and at least `MIN_CHANGED_CELLS` of them. A blinking cursor, a ticking clock
/// or a tray icon touches one to a few cells.
pub const CHANGED_PERMILLE: u32 = 20;
pub const MIN_CHANGED_CELLS: u32 = 6;

/// Byte budget on the sealed channel: refill per second and burst.
pub const BYTES_PER_SECOND: u64 = 48 * 1024;
pub const BURST_BYTES: u64 = 160 * 1024;
/// Largest JPEG sent (the relay caps one ctl message at about 190 KB of JSON).
pub const MAX_JPEG_BYTES: usize = 96 * 1024;

/// A downscaled picture: BGRA, tightly packed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thumb {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

impl Thumb {
    pub fn new(width: u32, height: u32, bgra: Vec<u8>) -> Result<Self> {
        ensure!(
            width >= 1
                && height >= 1
                && bgra.len() as u64 == u64::from(width) * u64::from(height) * 4,
            "thumbnail pixel buffer does not match {width}x{height}"
        );
        Ok(Self {
            width,
            height,
            bgra,
        })
    }

    fn luma(&self, x: u32, y: u32) -> u32 {
        let i = ((y * self.width + x) * 4) as usize;
        let (b, g, r) = (
            u32::from(self.bgra[i]),
            u32::from(self.bgra[i + 1]),
            u32::from(self.bgra[i + 2]),
        );
        (29 * b + 150 * g + 77 * r) >> 8
    }
}

/// Target size of a snapshot for a monitor of `width` x `height`.
pub fn preview_size(width: u32, height: u32) -> (u32, u32) {
    if width == 0 || height == 0 {
        return (MIN_WIDTH, PREVIEW_HEIGHT);
    }
    if height <= PREVIEW_HEIGHT {
        return (width.clamp(MIN_WIDTH, MAX_WIDTH), height);
    }
    let w =
        (u64::from(width) * u64::from(PREVIEW_HEIGHT) + u64::from(height) / 2) / u64::from(height);
    ((w as u32).clamp(MIN_WIDTH, MAX_WIDTH), PREVIEW_HEIGHT)
}

/// Cells whose mean luma moved by at least `CELL_MEAN_DELTA`, and the total cell count.
/// Different sizes count as everything changed.
pub fn changed_cells(previous: &Thumb, current: &Thumb) -> (u32, u32) {
    let columns = current.width.div_ceil(CELL);
    let rows = current.height.div_ceil(CELL);
    let total = columns * rows;
    if previous.width != current.width || previous.height != current.height {
        return (total, total);
    }
    let mut changed = 0;
    for row in 0..rows {
        for column in 0..columns {
            let x0 = column * CELL;
            let y0 = row * CELL;
            let x1 = (x0 + CELL).min(current.width);
            let y1 = (y0 + CELL).min(current.height);
            let mut sum = 0u64;
            for y in y0..y1 {
                for x in x0..x1 {
                    sum += u64::from(previous.luma(x, y).abs_diff(current.luma(x, y)));
                }
            }
            let pixels = u64::from((x1 - x0) * (y1 - y0));
            if sum / pixels >= u64::from(CELL_MEAN_DELTA) {
                changed += 1;
            }
        }
    }
    (changed, total)
}

/// Whether `current` differs from the last snapshot sent enough to send a new one.
pub fn is_significant_change(previous: &Thumb, current: &Thumb) -> bool {
    let (changed, total) = changed_cells(previous, current);
    changed >= MIN_CHANGED_CELLS && changed * 1000 >= total * CHANGED_PERMILLE
}

/// Encodes a thumbnail as a baseline JPEG.
pub fn encode_jpeg(thumb: &Thumb) -> Result<Vec<u8>> {
    let mut rgb = Vec::with_capacity((thumb.width * thumb.height * 3) as usize);
    for pixel in thumb.bgra.chunks_exact(4) {
        rgb.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
    }
    let mut out = Vec::new();
    let encoder = jpeg_encoder::Encoder::new(&mut out, JPEG_QUALITY);
    encoder
        .encode(
            &rgb,
            u16::try_from(thumb.width).context("thumbnail width")?,
            u16::try_from(thumb.height).context("thumbnail height")?,
            jpeg_encoder::ColorType::Rgb,
        )
        .context("encode monitor preview JPEG")?;
    Ok(out)
}

/// The sealed ctl message for one snapshot.
pub fn message(monitor: &str, seq: u64, width: u32, height: u32, jpeg: &[u8]) -> Value {
    json!({
        "type": MESSAGE_TYPE,
        "v": WIRE_VERSION,
        "monitor": monitor,
        "seq": seq,
        "w": width,
        "h": height,
        "jpeg": base64::engine::general_purpose::STANDARD.encode(jpeg),
    })
}

/// Why nothing was sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    Unchanged,
    Gated,
    Budget,
    TooLarge,
}

/// Link conditions that pause snapshots. The main loop fills this from what it already knows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LinkState {
    pub stable: bool,
}

/// Quiet time after the link was last seen unstable (not live, constrained, reconnected or
/// lowering quality) before snapshots resume.
pub const QUIET_AFTER_TROUBLE: Duration = Duration::from_secs(10);
/// Quiet time after a monitor switch: the live stream's first frames come first.
pub const QUIET_AFTER_SWITCH: Duration = Duration::from_secs(3);

/// What the main loop knows about the link each tick.
#[derive(Debug, Clone, Copy)]
pub struct LinkSample {
    /// A viewer's media transport is up.
    pub live: bool,
    /// A monitor switch is still waiting for its first frame.
    pub switching: bool,
    /// The link reports a constrained capacity (tight bandwidth).
    pub constrained: bool,
    /// Monotonic count of the media link's reconnects.
    pub reconnects: u64,
    /// The encoder's target bitrate.
    pub target_bitrate: u32,
}

/// Turns [`LinkSample`]s into the stable/not stable answer of [`LinkState`].
#[derive(Debug, Default)]
pub struct StableLink {
    quiet_until: Option<Instant>,
    reconnects: Option<u64>,
    target_bitrate: Option<u32>,
}

impl StableLink {
    /// A monitor switch just happened.
    pub fn note_switch(&mut self, now: Instant) {
        self.quiet_for(now, QUIET_AFTER_SWITCH);
    }

    fn quiet_for(&mut self, now: Instant, span: Duration) {
        let until = now + span;
        if self.quiet_until.is_none_or(|at| at < until) {
            self.quiet_until = Some(until);
        }
    }

    pub fn observe(&mut self, now: Instant, sample: LinkSample) -> LinkState {
        let reconnected = self
            .reconnects
            .is_some_and(|before| sample.reconnects != before);
        let lowered = self
            .target_bitrate
            .is_some_and(|before| sample.target_bitrate < before);
        self.reconnects = Some(sample.reconnects);
        self.target_bitrate = Some(sample.target_bitrate);
        if !sample.live || sample.constrained || reconnected || lowered {
            self.quiet_for(now, QUIET_AFTER_TROUBLE);
        }
        let quiet = self.quiet_until.is_some_and(|at| now < at);
        LinkState {
            stable: sample.live && !sample.switching && !quiet,
        }
    }
}

/// Counters logged once at session end.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    pub sent: u64,
    pub bytes: u64,
    pub skipped_unchanged: u64,
    pub skipped_gated: u64,
    pub skipped_budget: u64,
}

/// Token bucket in bytes.
struct Budget {
    tokens: u64,
    last_refill: Instant,
}

impl Budget {
    fn new(now: Instant) -> Self {
        Self {
            tokens: BURST_BYTES,
            last_refill: now,
        }
    }

    fn try_spend(&mut self, now: Instant, bytes: u64) -> bool {
        let elapsed = now.saturating_duration_since(self.last_refill);
        self.last_refill = now;
        self.tokens = (self.tokens + (elapsed.as_secs_f64() * BYTES_PER_SECOND as f64) as u64)
            .min(BURST_BYTES);
        if bytes > self.tokens {
            return false;
        }
        self.tokens -= bytes;
        true
    }
}

struct MonitorState {
    last_sent: Option<Thumb>,
    last_checked: Option<Instant>,
}

/// Per-session sender state. One snapshot is sent per monitor per second at most, only when the
/// monitor changed significantly since the last snapshot sent (the first is always sent),
/// only while the link is stable, and within a byte budget.
pub struct PreviewSender {
    monitors: HashMap<String, MonitorState>,
    budget: Budget,
    seq: u64,
    stats: Stats,
}

impl PreviewSender {
    pub fn new(now: Instant) -> Self {
        Self {
            monitors: HashMap::new(),
            budget: Budget::new(now),
            seq: 0,
            stats: Stats::default(),
        }
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// A monitor that is not streamed is due for a picture when the interval has passed.
    pub fn due(&self, monitor: &str, now: Instant) -> bool {
        self.monitors
            .get(monitor)
            .and_then(|state| state.last_checked)
            .is_none_or(|at| now.duration_since(at) >= CHECK_INTERVAL)
    }

    /// A picture was not taken (link not stable, or the capture failed): the interval restarts
    /// and the gate is counted. The first picture of the monitor is still ahead.
    pub fn gated(&mut self, monitor: &str, now: Instant) {
        self.monitors
            .entry(monitor.to_owned())
            .or_insert(MonitorState {
                last_sent: None,
                last_checked: None,
            })
            .last_checked = Some(now);
        self.stats.skipped_gated += 1;
    }

    /// Forgets monitors that are gone and the one now streamed (a later switch away from it
    /// starts with a fresh snapshot).
    pub fn retain(&mut self, others: &[String]) {
        self.monitors.retain(|name, _| others.contains(name));
    }

    /// Decides on a fresh picture of `monitor`: returns the JPEG to send, or why not. The caller
    /// sends it right away and reports nothing back: a snapshot lost on the way is replaced by
    /// the next significant change.
    pub fn consider(
        &mut self,
        monitor: &str,
        thumb: Thumb,
        link: LinkState,
        now: Instant,
    ) -> std::result::Result<(u64, Vec<u8>), Skip> {
        let state = self
            .monitors
            .entry(monitor.to_owned())
            .or_insert(MonitorState {
                last_sent: None,
                last_checked: None,
            });
        state.last_checked = Some(now);
        if !link.stable {
            self.stats.skipped_gated += 1;
            return Err(Skip::Gated);
        }
        if let Some(previous) = &state.last_sent {
            if !is_significant_change(previous, &thumb) {
                self.stats.skipped_unchanged += 1;
                return Err(Skip::Unchanged);
            }
        }
        let jpeg = match encode_jpeg(&thumb) {
            Ok(jpeg) if jpeg.len() <= MAX_JPEG_BYTES => jpeg,
            _ => return Err(Skip::TooLarge),
        };
        let cost = jpeg.len() as u64;
        if !self.budget.try_spend(now, cost) {
            self.stats.skipped_budget += 1;
            return Err(Skip::Budget);
        }
        state.last_sent = Some(thumb);
        self.seq += 1;
        self.stats.sent += 1;
        self.stats.bytes += cost;
        Ok((self.seq, jpeg))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(width: u32, height: u32, value: u8) -> Thumb {
        Thumb::new(width, height, vec![value; (width * height * 4) as usize]).unwrap()
    }

    fn paint(thumb: &mut Thumb, x0: u32, y0: u32, w: u32, h: u32, value: u8) {
        for y in y0..y0 + h {
            for x in x0..x0 + w {
                let i = ((y * thumb.width + x) * 4) as usize;
                thumb.bgra[i..i + 3].fill(value);
            }
        }
    }

    #[test]
    fn size_follows_the_aspect_ratio_at_360p() {
        assert_eq!(preview_size(1920, 1080), (640, 360));
        assert_eq!(preview_size(2560, 1440), (640, 360));
        assert_eq!(preview_size(3440, 1440), (860, 360));
        assert_eq!(preview_size(1080, 1920), (203, 360));
        assert_eq!(preview_size(800, 300), (800, 300));
        assert_eq!(preview_size(0, 0), (MIN_WIDTH, PREVIEW_HEIGHT));
    }

    #[test]
    fn identical_pictures_are_not_a_change() {
        let a = flat(640, 360, 40);
        assert_eq!(changed_cells(&a, &a).0, 0);
        assert!(!is_significant_change(&a, &a));
    }

    #[test]
    fn a_cursor_a_clock_and_a_tray_icon_are_not_significant() {
        let a = flat(640, 360, 40);
        let mut b = a.clone();
        paint(&mut b, 100, 100, 2, 18, 255); // blinking text cursor
        paint(&mut b, 560, 340, 64, 16, 255); // ticking clock: four cells wide
        paint(&mut b, 620, 344, 16, 16, 200); // tray icon
        let (changed, total) = changed_cells(&a, &b);
        assert!(
            changed > 0 && changed * 1000 < total * CHANGED_PERMILLE,
            "{changed}/{total}"
        );
        assert!(!is_significant_change(&a, &b));
    }

    #[test]
    fn a_window_opening_is_significant() {
        let a = flat(640, 360, 40);
        let mut b = a.clone();
        paint(&mut b, 80, 60, 320, 200, 230);
        assert!(is_significant_change(&a, &b));
    }

    #[test]
    fn a_new_size_always_counts_as_changed() {
        assert!(is_significant_change(
            &flat(640, 360, 1),
            &flat(860, 360, 1)
        ));
    }

    #[test]
    fn jpeg_is_a_valid_small_baseline_image() {
        let mut t = flat(640, 360, 200);
        paint(&mut t, 80, 60, 320, 200, 20);
        let jpeg = encode_jpeg(&t).unwrap();
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8]);
        assert_eq!(&jpeg[jpeg.len() - 2..], &[0xFF, 0xD9]);
        assert!(jpeg.len() < 20_000, "{}", jpeg.len());
    }

    #[test]
    fn message_carries_the_fields_clients_read() {
        let m = message(r"\\.\DISPLAY2", 7, 640, 360, &[1, 2, 3]);
        assert_eq!(m["type"], MESSAGE_TYPE);
        assert_eq!(m["v"], 1);
        assert_eq!(m["monitor"], r"\\.\DISPLAY2");
        assert_eq!(m["seq"], 7);
        assert_eq!((m["w"].as_u64(), m["h"].as_u64()), (Some(640), Some(360)));
        assert_eq!(m["jpeg"], "AQID");
    }

    fn busy(width: u32, height: u32, shade: u8) -> Thumb {
        let mut t = flat(width, height, 30);
        paint(&mut t, 20, 20, 400, 250, shade);
        t
    }

    #[test]
    fn the_first_snapshot_is_always_sent_then_unchanged_is_skipped() {
        let t0 = Instant::now();
        let mut sender = PreviewSender::new(t0);
        let link = LinkState { stable: true };
        let first = busy(640, 360, 200);
        assert!(sender.consider("m2", first.clone(), link, t0).is_ok());
        let again = sender.consider("m2", first.clone(), link, t0 + Duration::from_secs(1));
        assert_eq!(again.unwrap_err(), Skip::Unchanged);
        let changed = busy(640, 360, 90);
        assert!(sender
            .consider("m2", changed, link, t0 + Duration::from_secs(2))
            .is_ok());
        let stats = sender.stats();
        assert_eq!((stats.sent, stats.skipped_unchanged), (2, 1));
        assert!(stats.bytes > 0);
    }

    #[test]
    fn nothing_is_sent_while_the_link_is_not_stable_and_the_first_one_waits() {
        let t0 = Instant::now();
        let mut sender = PreviewSender::new(t0);
        let t = busy(640, 360, 200);
        let unstable = LinkState { stable: false };
        assert_eq!(
            sender.consider("m2", t.clone(), unstable, t0).unwrap_err(),
            Skip::Gated
        );
        assert_eq!(sender.stats().sent, 0);
        // Once the link is stable the same picture is still the first snapshot.
        assert!(sender
            .consider(
                "m2",
                t,
                LinkState { stable: true },
                t0 + Duration::from_secs(1)
            )
            .is_ok());
        assert_eq!(sender.stats().skipped_gated, 1);
    }

    #[test]
    fn each_monitor_has_its_own_interval_and_baseline() {
        let t0 = Instant::now();
        let mut sender = PreviewSender::new(t0);
        assert!(sender.due("m2", t0));
        let link = LinkState { stable: true };
        let _ = sender.consider("m2", busy(640, 360, 200), link, t0);
        assert!(!sender.due("m2", t0 + Duration::from_millis(900)));
        assert!(sender.due("m2", t0 + Duration::from_secs(1)));
        assert!(sender.due("m3", t0));
        assert!(sender.consider("m3", busy(640, 360, 200), link, t0).is_ok());
    }

    #[test]
    fn the_byte_budget_allows_a_burst_then_the_refill_rate() {
        let t0 = Instant::now();
        let mut budget = Budget::new(t0);
        assert!(budget.try_spend(t0, BURST_BYTES));
        assert!(!budget.try_spend(t0, 1));
        let second = t0 + Duration::from_secs(1);
        assert!(!budget.try_spend(second, BYTES_PER_SECOND + 1));
        assert!(budget.try_spend(second, BYTES_PER_SECOND - 100));
        // Idle time never banks more than the burst.
        let later = second + Duration::from_secs(3600);
        assert!(budget.try_spend(later, BURST_BYTES));
        assert!(!budget.try_spend(later, 1));
    }

    fn sample() -> LinkSample {
        LinkSample {
            live: true,
            switching: false,
            constrained: false,
            reconnects: 0,
            target_bitrate: 8_000_000,
        }
    }

    #[test]
    fn the_link_is_stable_only_after_the_quiet_time_following_trouble() {
        let t0 = Instant::now();
        let mut link = StableLink::default();
        // Not live yet: unstable, and the quiet time starts.
        let down = LinkSample {
            live: false,
            ..sample()
        };
        assert!(!link.observe(t0, down).stable);
        assert!(!link.observe(t0 + Duration::from_secs(5), sample()).stable);
        assert!(link.observe(t0 + QUIET_AFTER_TROUBLE, sample()).stable);
    }

    fn trouble_pauses_for_the_quiet_time(trouble: LinkSample) {
        let t0 = Instant::now();
        let mut link = StableLink::default();
        assert!(link.observe(t0, sample()).stable);
        let t1 = t0 + Duration::from_secs(20);
        assert!(!link.observe(t1, trouble).stable, "{trouble:?}");
        let calm = LinkSample {
            constrained: false,
            ..trouble
        };
        assert!(
            !link
                .observe(t1 + QUIET_AFTER_TROUBLE - Duration::from_secs(1), calm)
                .stable
        );
        assert!(link.observe(t1 + QUIET_AFTER_TROUBLE, calm).stable);
    }

    #[test]
    fn a_reconnect_a_constrained_link_and_a_quality_drop_each_pause_snapshots() {
        trouble_pauses_for_the_quiet_time(LinkSample {
            reconnects: 1,
            ..sample()
        });
        trouble_pauses_for_the_quiet_time(LinkSample {
            constrained: true,
            ..sample()
        });
        trouble_pauses_for_the_quiet_time(LinkSample {
            target_bitrate: 4_000_000,
            ..sample()
        });
    }

    #[test]
    fn a_switch_pauses_until_its_first_frame_and_a_moment_after() {
        let t0 = Instant::now();
        let mut link = StableLink::default();
        assert!(link.observe(t0 + QUIET_AFTER_TROUBLE, sample()).stable);
        let now = t0 + QUIET_AFTER_TROUBLE;
        link.note_switch(now);
        let waiting = LinkSample {
            switching: true,
            ..sample()
        };
        assert!(!link.observe(now + Duration::from_secs(1), waiting).stable);
        assert!(!link.observe(now + Duration::from_secs(2), sample()).stable);
        assert!(link.observe(now + QUIET_AFTER_SWITCH, sample()).stable);
    }

    #[test]
    fn retain_forgets_monitors_that_left_the_list() {
        let t0 = Instant::now();
        let mut sender = PreviewSender::new(t0);
        let link = LinkState { stable: true };
        let _ = sender.consider("m2", busy(640, 360, 200), link, t0);
        sender.retain(&["m3".to_owned()]);
        assert!(sender.due("m2", t0));
    }
}
