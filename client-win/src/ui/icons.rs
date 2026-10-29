//! Lucide icons (lucide-static@0.460.0, ISC), rasterised with resvg.
//!
//! The SVGs in src/ui/icons/ are verbatim copies of the Lucide files; the
//! licence is LICENSE-lucide.txt next to them. Each one is embedded, rasterised
//! once to white premultiplied RGBA at RASTER_PX square, and uploaded as an
//! egui texture. Drawing applies the caller's colour as a tint, which is exact
//! because the raster is white.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use egui::{
    load::SizedTexture, pos2, vec2, Color32, ColorImage, Context, Painter, Rect, TextureHandle,
    TextureId, TextureOptions,
};
use resvg::{tiny_skia, usvg};

/// Side of the rasterised icon, in pixels (enough for 16-22 pt at 2x).
const RASTER_PX: u32 = 64;

/// (Lucide name, SVG source). Generated from src/ui/icons/*.svg.
const ICONS: &[(&str, &str)] = &[
    ("activity", include_str!("icons/activity.svg")),
    (
        "arrow-left-right",
        include_str!("icons/arrow-left-right.svg"),
    ),
    ("arrow-right", include_str!("icons/arrow-right.svg")),
    ("badge-check", include_str!("icons/badge-check.svg")),
    ("ban", include_str!("icons/ban.svg")),
    ("bell-ring", include_str!("icons/bell-ring.svg")),
    ("briefcase", include_str!("icons/briefcase.svg")),
    ("check", include_str!("icons/check.svg")),
    ("chevron-down", include_str!("icons/chevron-down.svg")),
    ("chevron-left", include_str!("icons/chevron-left.svg")),
    ("chevron-up", include_str!("icons/chevron-up.svg")),
    (
        "chevrons-up-down",
        include_str!("icons/chevrons-up-down.svg"),
    ),
    ("circle-alert", include_str!("icons/circle-alert.svg")),
    ("circle-check", include_str!("icons/circle-check.svg")),
    ("circle-stop", include_str!("icons/circle-stop.svg")),
    ("clipboard", include_str!("icons/clipboard.svg")),
    ("clock", include_str!("icons/clock.svg")),
    ("cloud", include_str!("icons/cloud.svg")),
    ("copy", include_str!("icons/copy.svg")),
    ("cpu", include_str!("icons/cpu.svg")),
    ("download", include_str!("icons/download.svg")),
    ("ellipsis", include_str!("icons/ellipsis.svg")),
    ("external-link", include_str!("icons/external-link.svg")),
    ("eye", include_str!("icons/eye.svg")),
    ("eye-off", include_str!("icons/eye-off.svg")),
    ("file-key", include_str!("icons/file-key.svg")),
    ("file-up", include_str!("icons/file-up.svg")),
    ("fingerprint", include_str!("icons/fingerprint.svg")),
    ("folder-open", include_str!("icons/folder-open.svg")),
    ("gamepad-2", include_str!("icons/gamepad-2.svg")),
    ("gauge", include_str!("icons/gauge.svg")),
    ("globe", include_str!("icons/globe.svg")),
    ("history", include_str!("icons/history.svg")),
    ("hourglass", include_str!("icons/hourglass.svg")),
    ("house", include_str!("icons/house.svg")),
    ("info", include_str!("icons/info.svg")),
    ("key-round", include_str!("icons/key-round.svg")),
    ("key-square", include_str!("icons/key-square.svg")),
    ("keyboard", include_str!("icons/keyboard.svg")),
    ("keyboard-off", include_str!("icons/keyboard-off.svg")),
    ("laptop", include_str!("icons/laptop.svg")),
    ("link", include_str!("icons/link.svg")),
    ("loader-circle", include_str!("icons/loader-circle.svg")),
    ("lock", include_str!("icons/lock.svg")),
    ("log-out", include_str!("icons/log-out.svg")),
    ("maximize-2", include_str!("icons/maximize-2.svg")),
    ("minimize-2", include_str!("icons/minimize-2.svg")),
    ("minus", include_str!("icons/minus.svg")),
    ("monitor", include_str!("icons/monitor.svg")),
    ("monitor-check", include_str!("icons/monitor-check.svg")),
    ("monitor-play", include_str!("icons/monitor-play.svg")),
    ("mouse", include_str!("icons/mouse.svg")),
    ("mouse-pointer-2", include_str!("icons/mouse-pointer-2.svg")),
    (
        "mouse-pointer-click",
        include_str!("icons/mouse-pointer-click.svg"),
    ),
    ("pause", include_str!("icons/pause.svg")),
    ("pencil", include_str!("icons/pencil.svg")),
    ("pin", include_str!("icons/pin.svg")),
    ("play", include_str!("icons/play.svg")),
    ("plus", include_str!("icons/plus.svg")),
    ("power", include_str!("icons/power.svg")),
    ("qr-code", include_str!("icons/qr-code.svg")),
    ("radio", include_str!("icons/radio.svg")),
    ("radio-tower", include_str!("icons/radio-tower.svg")),
    ("refresh-cw", include_str!("icons/refresh-cw.svg")),
    ("rotate-ccw", include_str!("icons/rotate-ccw.svg")),
    ("router", include_str!("icons/router.svg")),
    ("server", include_str!("icons/server.svg")),
    ("settings", include_str!("icons/settings.svg")),
    ("shield-alert", include_str!("icons/shield-alert.svg")),
    ("shield-check", include_str!("icons/shield-check.svg")),
    ("shield-off", include_str!("icons/shield-off.svg")),
    ("shield-x", include_str!("icons/shield-x.svg")),
    ("signal", include_str!("icons/signal.svg")),
    ("smartphone", include_str!("icons/smartphone.svg")),
    ("square", include_str!("icons/square.svg")),
    ("timer", include_str!("icons/timer.svg")),
    ("trash-2", include_str!("icons/trash-2.svg")),
    ("undo-2", include_str!("icons/undo-2.svg")),
    ("unplug", include_str!("icons/unplug.svg")),
    ("upload", include_str!("icons/upload.svg")),
    ("user-check", include_str!("icons/user-check.svg")),
    ("volume-2", include_str!("icons/volume-2.svg")),
    ("volume-x", include_str!("icons/volume-x.svg")),
    ("wifi-off", include_str!("icons/wifi-off.svg")),
    ("x", include_str!("icons/x.svg")),
];

pub struct Icons {
    textures: HashMap<&'static str, TextureHandle>,
    /// One transparent pixel, drawn for unknown names so the layout holds.
    blank: TextureHandle,
    /// Unknown names already reported on stderr.
    warned: Mutex<HashSet<String>>,
}

impl Icons {
    /// Rasterise every bundled icon into egui textures (white, tinted at
    /// draw time). Called once on the first frame.
    pub fn load(ctx: &Context) -> Self {
        let mut textures = HashMap::new();
        for (name, svg) in ICONS {
            match rasterise(svg) {
                Ok(img) => {
                    let handle =
                        ctx.load_texture(format!("icon:{name}"), img, TextureOptions::LINEAR);
                    textures.insert(*name, handle);
                }
                Err(e) => eprintln!("icon {name} failed to rasterise: {e}"),
            }
        }
        let blank = ctx.load_texture(
            "icon:blank",
            ColorImage::new([1, 1], vec![Color32::TRANSPARENT]),
            TextureOptions::LINEAR,
        );
        Self {
            textures,
            blank,
            warned: Mutex::new(HashSet::new()),
        }
    }

    /// The texture for `name`, or None (logged once) for an unknown name.
    fn texture(&self, name: &str) -> Option<TextureId> {
        if let Some(handle) = self.textures.get(name) {
            return Some(handle.id());
        }
        if let Ok(mut warned) = self.warned.lock() {
            if warned.insert(name.to_owned()) {
                eprintln!("unknown icon name: {name}");
            }
        }
        None
    }

    /// An image of icon `name` (Lucide name, e.g. "monitor-play") at
    /// `size` points, tinted `color`. Unknown names log once and draw
    /// nothing.
    pub fn image(&self, name: &str, size: f32, color: Color32) -> egui::Image<'static> {
        match self.texture(name) {
            Some(id) => egui::Image::new(SizedTexture::new(id, vec2(size, size))).tint(color),
            None => egui::Image::new(SizedTexture::new(self.blank.id(), vec2(size, size)))
                .tint(Color32::TRANSPARENT),
        }
    }

    /// Paint icon `name` into `rect` with `color`.
    pub fn paint(&self, painter: &Painter, rect: Rect, name: &str, color: Color32) {
        if let Some(id) = self.texture(name) {
            let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
            painter.image(id, rect, uv, color);
        }
    }
}

/// Parse one Lucide SVG with `currentColor` set to white and render it to
/// premultiplied RGBA at RASTER_PX square.
fn rasterise(svg: &str) -> Result<ColorImage, String> {
    let white = svg.replace("currentColor", "#ffffff");
    let tree =
        usvg::Tree::from_str(&white, &usvg::Options::default()).map_err(|e| e.to_string())?;
    let size = tree.size();
    let mut pixmap = tiny_skia::Pixmap::new(RASTER_PX, RASTER_PX)
        .ok_or_else(|| "zero-size pixmap".to_owned())?;
    let transform = tiny_skia::Transform::from_scale(
        RASTER_PX as f32 / size.width(),
        RASTER_PX as f32 / size.height(),
    );
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    let side = RASTER_PX as usize;
    Ok(ColorImage::from_rgba_premultiplied(
        [side, side],
        pixmap.data(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_rasterises_with_ink() {
        assert!(!ICONS.is_empty());
        for (name, svg) in ICONS {
            let img = rasterise(svg).unwrap_or_else(|e| panic!("{name}: {e}"));
            let inked = img.pixels.iter().filter(|p| p.a() > 0).count();
            assert!(inked > 0, "{name}: no non-transparent pixels");
        }
    }
}
