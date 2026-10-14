//! CPU-only pressure controls. Resolution falls before the frame-rate cap.
//! Decisions use measured work, consecutive pressure and a dwell time; source
//! pixels remain owned by capture and profile/IDR transactions remain host-owned.
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuTarget {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

pub struct CpuPacing {
    adaptive: bool,
    fps_adaptive: bool,
    fixed_height: Option<u32>,
    fixed_fps: Option<u32>,
    source: (u32, u32),
    ceiling_height: u32,
    rung: usize,
    low_fps: bool,
    average_us: f64,
    pressured: u32,
    spare: u32,
    changed_at: Instant,
}

fn enabled(name: &str, default: bool) -> bool {
    std::env::var(name).map_or(default, |v| v != "0")
}

impl CpuPacing {
    pub fn from_env(now: Instant) -> Self {
        Self {
            adaptive: enabled("BP_CPU_ADAPTIVE", true),
            fps_adaptive: enabled("BP_CPU_ADAPTIVE_FPS", true),
            fixed_height: std::env::var("BP_CPU_MAX_HEIGHT")
                .ok()
                .and_then(|v| v.parse().ok())
                .filter(|h| *h >= 2),
            fixed_fps: std::env::var("BP_CPU_MAX_FPS")
                .ok()
                .and_then(|v| v.parse().ok())
                .filter(|f| *f > 0),
            source: (0, 0),
            ceiling_height: 0,
            rung: 0,
            low_fps: false,
            average_us: 0.0,
            pressured: 0,
            spare: 0,
            changed_at: now,
        }
    }

    fn heights(&self) -> Vec<u32> {
        let native = self.ceiling_height;
        let mut heights = vec![native];
        for height in [720, 540] {
            if height < *heights.last().unwrap() {
                heights.push(height);
            }
        }
        heights
    }

    /// Clamp an existing requested output; never upscale or override a smaller
    /// viewer-selected profile. Native aspect ratio is retained when capped.
    pub fn target(&mut self, source: (u32, u32), requested: CpuTarget, now: Instant) -> CpuTarget {
        let width_limited_height =
            (u64::from(requested.width) * u64::from(source.1) / u64::from(source.0.max(1))) as u32;
        let ceiling_height = requested
            .height
            .min(source.1)
            .min(width_limited_height)
            .min(self.fixed_height.unwrap_or(u32::MAX))
            & !1;
        if self.source != source || self.ceiling_height != ceiling_height.max(2) {
            self.source = source;
            self.ceiling_height = ceiling_height.max(2);
            self.rung = 0;
            self.low_fps = false;
            self.average_us = 0.0;
            self.pressured = 0;
            self.spare = 0;
            self.changed_at = now;
        }
        let cap_height = self.heights()[self.rung].min(self.fixed_height.unwrap_or(u32::MAX));
        let height = requested
            .height
            .min(source.1)
            .min(cap_height)
            .min(width_limited_height)
            & !1;
        let width = ((u64::from(source.0) * u64::from(height) / u64::from(source.1.max(1))) as u32)
            .min(requested.width)
            & !1;
        CpuTarget {
            width: width.max(2),
            height: height.max(2),
            fps: requested
                .fps
                .min(self.fixed_fps.unwrap_or(u32::MAX))
                .min(if self.low_fps { 30 } else { u32::MAX })
                .max(1),
        }
    }

    /// Call only for frames actually encoded, using conversion + encode +
    /// packing cost. Return true when the next frame needs a new target.
    pub fn observe(&mut self, work_us: u64, target: CpuTarget, now: Instant) -> bool {
        if !self.adaptive {
            return false;
        }
        self.average_us = if self.average_us == 0.0 {
            work_us as f64
        } else {
            self.average_us * 0.9 + work_us as f64 * 0.1
        };
        let budget = 1_000_000.0 / target.fps.max(1) as f64;
        let heights = self.heights();
        let next_is_larger = self.rung > 0;
        let recovery_cost = if self.low_fps {
            self.average_us * 2.0
        } else if next_is_larger {
            self.average_us * (heights[self.rung - 1] as f64 / heights[self.rung] as f64).powi(2)
        } else {
            self.average_us
        };
        if self.average_us > budget * 0.9 {
            self.pressured += 1;
            self.spare = 0;
        } else if recovery_cost < budget * 0.60 {
            self.spare += 1;
            self.pressured = 0;
        } else {
            self.pressured = 0;
            self.spare = 0;
        }
        if now.saturating_duration_since(self.changed_at) < Duration::from_secs(3) {
            return false;
        }
        let changed = if self.pressured >= 24 {
            if self.rung + 1 < heights.len() {
                self.rung += 1;
                true
            } else if self.fps_adaptive && !self.low_fps && target.fps > 30 {
                self.low_fps = true;
                true
            } else {
                false
            }
        } else if self.spare >= 180
            && now.saturating_duration_since(self.changed_at) >= Duration::from_secs(10)
        {
            if self.low_fps {
                self.low_fps = false;
                true
            } else if self.rung > 0 {
                self.rung -= 1;
                true
            } else {
                false
            }
        } else {
            false
        };
        if changed {
            self.changed_at = now;
            self.pressured = 0;
            self.spare = 0;
            self.average_us = 0.0;
        }
        changed
    }
}

/// Retain all damage since the last successful conversion, including frames
/// rejected by pacing or transport admission. Unknown metadata forces refresh.
#[derive(Default)]
pub struct CpuDamage {
    geometry: Option<(u32, u32)>,
    full: bool,
    rect: Option<crate::frame_damage::DamageRect>,
}
impl CpuDamage {
    pub fn note(
        &mut self,
        geometry: (u32, u32),
        damage: Option<&[crate::frame_damage::DamageRect]>,
    ) {
        if self.geometry != Some(geometry) {
            self.geometry = Some(geometry);
            self.full = true;
        }
        match damage {
            None => self.full = true,
            Some(rects) => {
                for &r in rects {
                    if r.left >= r.right
                        || r.top >= r.bottom
                        || r.right > geometry.0
                        || r.bottom > geometry.1
                    {
                        self.full = true;
                    } else {
                        self.rect =
                            Some(self.rect.map_or(r, |a| crate::frame_damage::DamageRect {
                                left: a.left.min(r.left),
                                top: a.top.min(r.top),
                                right: a.right.max(r.right),
                                bottom: a.bottom.max(r.bottom),
                            }));
                    }
                }
            }
        }
    }
    pub fn rects(&self) -> Option<&[crate::frame_damage::DamageRect]> {
        if self.full {
            None
        } else {
            Some(self.rect.as_slice())
        }
    }
    pub fn converted(&mut self) {
        self.full = false;
        self.rect = None;
    }
    pub fn invalidate(&mut self) {
        self.full = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pressure_reduces_resolution_then_fps_and_hysteresis_prevents_flapping() {
        let start = Instant::now();
        let mut p = CpuPacing::from_env(start);
        p.adaptive = true;
        p.fps_adaptive = true;
        p.fixed_height = None;
        p.fixed_fps = None;
        let request = CpuTarget {
            width: 1920,
            height: 1080,
            fps: 60,
        };
        let mut now = start;
        for expected in [(1280, 720, 60), (960, 540, 60), (960, 540, 30)] {
            let target = p.target((1920, 1080), request, now);
            now += Duration::from_secs(4);
            for _ in 0..23 {
                assert!(!p.observe(25_000, target, now));
            }
            assert!(p.observe(25_000, target, now));
            let next = p.target((1920, 1080), request, now);
            assert_eq!((next.width, next.height, next.fps), expected);
        }
        let target = p.target((1920, 1080), request, now);
        for _ in 0..200 {
            assert!(!p.observe(1_000, target, now + Duration::from_secs(1)));
        }
        assert!(p.observe(1_000, target, now + Duration::from_secs(11)));
        assert_eq!(p.target((1920, 1080), request, now).fps, 60);
    }
    #[test]
    fn accumulated_damage_covers_discarded_frames_and_unknowns_refresh() {
        use crate::frame_damage::DamageRect;
        let mut d = CpuDamage::default();
        let a = DamageRect {
            left: 1,
            top: 2,
            right: 10,
            bottom: 12,
        };
        let b = DamageRect {
            left: 80,
            top: 20,
            right: 99,
            bottom: 31,
        };
        d.note((100, 100), Some(&[a]));
        assert!(d.rects().is_none());
        d.converted();
        d.note((100, 100), Some(&[a]));
        d.note((100, 100), Some(&[b]));
        assert_eq!(
            d.rects().unwrap(),
            &[DamageRect {
                left: 1,
                top: 2,
                right: 99,
                bottom: 31
            }]
        );
        d.note((100, 100), None);
        assert!(d.rects().is_none());
        d.converted();
        assert_eq!(d.rects(), Some([].as_slice()));
        d.note((101, 100), Some(&[a]));
        assert!(d.rects().is_none());
    }

    #[test]
    fn cap_retains_native_aspect_and_does_not_upscale_small_sources() {
        let now = Instant::now();
        let mut p = CpuPacing::from_env(now);
        p.fixed_height = None;
        p.fixed_fps = None;
        let target = p.target(
            (1280, 800),
            CpuTarget {
                width: 960,
                height: 540,
                fps: 60,
            },
            now,
        );
        assert_eq!(
            target,
            CpuTarget {
                width: 864,
                height: 540,
                fps: 60
            }
        );
        let target = p.target(
            (640, 400),
            CpuTarget {
                width: 1920,
                height: 1080,
                fps: 60,
            },
            now,
        );
        assert_eq!(
            target,
            CpuTarget {
                width: 640,
                height: 400,
                fps: 60
            }
        );
    }

    #[test]
    fn already_small_requested_mode_reduces_fps_without_empty_scale_steps() {
        let now = Instant::now();
        let mut p = CpuPacing::from_env(now);
        p.adaptive = true;
        p.fps_adaptive = true;
        p.fixed_height = None;
        p.fixed_fps = None;
        let requested = CpuTarget {
            width: 864,
            height: 540,
            fps: 60,
        };
        let target = p.target((1280, 800), requested, now);
        for _ in 0..23 {
            assert!(!p.observe(25_000, target, now + Duration::from_secs(4)));
        }
        assert!(p.observe(25_000, target, now + Duration::from_secs(4)));
        assert_eq!(p.target((1280, 800), requested, now).fps, 30);
    }
}
