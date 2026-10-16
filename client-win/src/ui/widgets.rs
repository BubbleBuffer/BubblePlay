//! BubbleBuffer widgets for egui. The signatures here are the contract the
//! screens use; the looks follow client-win/mockups/mockup.css and the
//! screen styles in connect.html and session.html.
//!
//! Everything is drawn with painter primitives on purpose: egui's default
//! button, checkbox and separator looks are not the BubbleBuffer look.

use std::f32::consts::TAU;
use std::time::Duration;

use egui::text::{LayoutJob, TextFormat};
use egui::{
    pos2, vec2, Align, Align2, Color32, CornerRadius, CursorIcon, FontId, Id, Layout, Margin,
    Rect, Response, Sense, Shadow, Stroke, StrokeKind, Ui, Vec2, WidgetText,
};

use super::contract;
use super::theme;
use super::Cx;
use crate::model::{ConnStep, StepStatus};
use crate::shell::WindowCommand;

// Shapes the theme does not name. Radii come from the mockups' fitted
// values (kbd.key, .switch .trk, the brand dot); see the notes in each use.
const R_KEY: CornerRadius = CornerRadius { nw: 4, ne: 5, se: 4, sw: 5 };
const R_KNOB: CornerRadius = CornerRadius::same(3);
/// The brand dot and the step markers (2 3 2 3).
const R_DOT: CornerRadius = CornerRadius { nw: 2, ne: 3, se: 2, sw: 3 };
/// Caption close hover. The mockup uses #C42B1C; the theme has no red fill token.
const CLOSE_HOVER: Color32 = Color32::from_rgb(0xC4, 0x2B, 0x1C);
const CAPTION_W: f32 = 46.0;
const CAPTION_ICON: f32 = 15.0;
/// Caption buttons in the title bar: minimise, maximise/restore, close.
const CAPTION_COUNT: usize = 3;
/// Opacity of disabled controls (mockup: opacity .45).
const DISABLED_ALPHA: f32 = 0.45;
/// Lucide icon size used by the step lists (svg.i is 18 px; the 22 px
/// column holds it).
const STEP_COL: f32 = 22.0;

/// Global egui style: dark visuals, FIELD background, UI font as default
/// text style, spacing on the 4 px base, no default strokes that fight the
/// fitted radii.
pub fn install_style(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.override_text_color = Some(theme::INK);
    visuals.panel_fill = theme::FIELD;
    visuals.window_fill = theme::SURFACE;
    visuals.window_stroke = Stroke::new(1.0, theme::THREAD);
    visuals.window_corner_radius = theme::R_PANEL;
    visuals.menu_corner_radius = theme::R_FIELD;
    visuals.window_shadow = Shadow::NONE;
    visuals.popup_shadow = Shadow::NONE;
    visuals.extreme_bg_color = theme::SURFACE_RAISED;
    visuals.faint_bg_color = theme::SURFACE;
    visuals.code_bg_color = theme::SURFACE_MUTED;
    visuals.hyperlink_color = theme::FOCUS;
    visuals.selection.bg_fill = theme::SIGNAL.gamma_multiply(0.3);
    visuals.selection.stroke = Stroke::new(1.0, theme::FOCUS);

    let w = &mut visuals.widgets;
    w.noninteractive.bg_fill = theme::FIELD;
    w.noninteractive.weak_bg_fill = theme::FIELD;
    w.noninteractive.bg_stroke = Stroke::new(1.0, theme::THREAD);
    w.noninteractive.fg_stroke = Stroke::new(1.0, theme::INK_MUTED);
    w.noninteractive.corner_radius = theme::R_CONTROL;
    for state in [&mut w.inactive, &mut w.open] {
        state.bg_fill = theme::SURFACE_RAISED;
        state.weak_bg_fill = theme::SURFACE_RAISED;
        state.bg_stroke = Stroke::new(1.0, theme::THREAD_STRONG);
        state.fg_stroke = Stroke::new(1.0, theme::INK);
        state.corner_radius = theme::R_CONTROL;
    }
    for state in [&mut w.hovered, &mut w.active] {
        state.bg_fill = theme::SURFACE_MUTED;
        state.weak_bg_fill = theme::SURFACE_MUTED;
        state.bg_stroke = Stroke::new(1.0, theme::THREAD_STRONG);
        state.fg_stroke = Stroke::new(1.0, theme::INK);
        state.corner_radius = theme::R_CONTROL;
        state.expansion = 0.0;
    }

    ctx.all_styles_mut(|style| {
        style.visuals = visuals.clone();
        style.spacing.item_spacing = vec2(theme::SP2, theme::SP2);
        style.spacing.button_padding = vec2(theme::SP3, theme::SP2);
        // The minimum height of a row of widgets. egui's default use of the
        // control height (36) made every text line in a `horizontal` 36 px
        // tall, which broke the 4 px rhythm and pushed second lines out of
        // their cards. Buttons and fields set their own heights.
        style.spacing.interact_size.y = theme::LINE_H;
    });
}

/// The 36 px custom title bar: app mark + "BubblePlay", the context text,
/// the demo label (small, faint) when given, and the three caption buttons
/// (minimise, maximise/restore, close). Pushes its drag area and the
/// caption buttons into `cx.out.drag` / `cx.out.no_drag` and turns clicks
/// into `cx.out.command`.
pub fn titlebar(root: &mut egui::Ui, cx: &mut Cx, context: &str, demo_label: Option<&str>) {
    egui::Panel::top("titlebar")
        .exact_size(theme::TITLEBAR_H)
        .frame(egui::Frame::NONE)
        .show(root, |ui| {
            let bar = ui.max_rect();
            let painter = ui.painter().clone();
            painter.rect_filled(bar, CornerRadius::ZERO, theme::FIELD);
            painter.line_segment(
                [pos2(bar.left(), bar.bottom() - 0.5), pos2(bar.right(), bar.bottom() - 0.5)],
                Stroke::new(1.0, theme::THREAD),
            );
            cx.out.drag.push(bar);

            let cy = bar.center().y;
            let mut x = bar.left() + 14.0;
            let dot = Rect::from_center_size(pos2(x + 4.0, cy), Vec2::splat(8.0));
            painter.rect_filled(dot, R_DOT, theme::SIGNAL);
            x += 16.0;
            let app = contract::paint_text(
                ui,
                pos2(x, cy),
                Align2::LEFT_CENTER,
                "BubblePlay",
                theme::display(14.0),
                theme::INK,
            );
            x = app.right() + 12.0;
            let ctx_rect = contract::paint_text(
                ui,
                pos2(x, cy),
                Align2::LEFT_CENTER,
                context,
                theme::body(theme::size::SMALL),
                theme::INK_FAINT,
            );
            x = ctx_rect.right() + 12.0;
            if let Some(d) = demo_label {
                contract::paint_text(ui, pos2(x, cy), Align2::LEFT_CENTER, d, theme::mono(11.0), theme::INK_FAINT);
            }

            let maximise_icon = if cx.window.maximized { "copy" } else { "square" };
            let caps = [
                ("minus", WindowCommand::Minimize),
                (maximise_icon, WindowCommand::ToggleMaximize),
                ("x", WindowCommand::Close),
            ];
            for (i, (icon_name, command)) in caps.into_iter().enumerate() {
                let right_gap = (CAPTION_COUNT - 1 - i) as f32 * CAPTION_W;
                let rect = Rect::from_min_size(
                    pos2(bar.right() - CAPTION_W - right_gap, bar.top()),
                    vec2(CAPTION_W, bar.height()),
                );
                let resp = ui.interact(rect, Id::new(("caption", i)), Sense::click());
                let hovered = resp.hovered();
                let close = i == CAPTION_COUNT - 1;
                let (bg, fg) = if close && hovered {
                    (Some(CLOSE_HOVER), theme::INK)
                } else if hovered {
                    (Some(theme::SURFACE_MUTED), theme::INK)
                } else {
                    (None, theme::INK_MUTED)
                };
                if let Some(bg) = bg {
                    painter.rect_filled(rect, CornerRadius::ZERO, bg);
                }
                let icon_rect = Rect::from_center_size(rect.center(), Vec2::splat(CAPTION_ICON));
                cx.icons.paint(&painter, icon_rect, icon_name, fg);
                cx.out.no_drag.push(rect);
                contract::mark_widget(&resp, egui::WidgetType::Button, "caption", icon_name);
                if resp.clicked() {
                    cx.out.command = Some(command);
                }
            }
            contract::mark(ui.ctx(), ui.unique_id(), contract::ROW, "titlebar", bar);
        });
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BtnKind {
    /// Filled apricot: the one main action on a screen.
    Primary,
    /// Raised surface with a thread-strong border.
    Secondary,
    /// No fill; text plus optional icon.
    Ghost,
    /// Ghost with danger-coloured text (Forget, Revoke).
    Danger,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BtnSize {
    /// 32 px: inline row actions, footers (the smallest hit target).
    Small,
    /// 36 px: default.
    Normal,
    /// 44 px: the main action of a flow screen.
    Large,
}

fn btn_metrics(size: BtnSize) -> (f32, f32, f32) {
    match size {
        BtnSize::Small => (theme::CONTROL_SM_H, theme::size::SMALL, 12.0),
        BtnSize::Normal => (theme::CONTROL_H, theme::size::LABEL, 16.0),
        BtnSize::Large => (theme::CONTROL_LG_H, theme::size::BODY, 20.0),
    }
}

/// The natural width of a [`button`] (outside justified layouts).
pub fn button_width(ui: &Ui, size: BtnSize, icon: Option<&str>, label: &str) -> f32 {
    let (_, font_size, pad) = btn_metrics(size);
    let icon_w = if icon.is_some() { theme::ICON_SM + theme::SP2 } else { 0.0 };
    pad * 2.0 + icon_w + text_width(ui, label, theme::bold(font_size), theme::INK)
}

/// A button with an optional Lucide icon before the label.
pub fn button(ui: &mut Ui, cx: &Cx, kind: BtnKind, size: BtnSize, icon: Option<&str>, label: &str) -> Response {
    let enabled = ui.is_enabled();
    let (height, font_size, pad) = btn_metrics(size);
    let font = theme::bold(font_size);
    let text_w = text_width(ui, label, font.clone(), theme::INK);
    let icon_w = if icon.is_some() { theme::ICON_SM + theme::SP2 } else { 0.0 };
    // In a justified layout (the main action of a panel) the button takes
    // the full width, as the mockups' block buttons do.
    let natural = pad * 2.0 + icon_w + text_w;
    let width = if ui.layout().horizontal_justify() { ui.available_width().max(natural) } else { natural };
    let (rect, resp) = ui.allocate_exact_size(vec2(width, height), Sense::click());

    let hovered = enabled && resp.hovered();
    let pressed = enabled && resp.is_pointer_button_down_on();
    let (fill, border, fg) = match kind {
        BtnKind::Primary => {
            let fill = if pressed {
                theme::SIGNAL_PRESS
            } else if hovered {
                theme::SIGNAL.lerp_to_gamma(theme::INK, 0.08)
            } else {
                theme::SIGNAL
            };
            (fill, fill, theme::SIGNAL_INK)
        }
        BtnKind::Secondary => {
            let fill = if pressed || hovered { theme::SURFACE_MUTED } else { theme::SURFACE_RAISED };
            (fill, theme::THREAD_STRONG, theme::INK)
        }
        BtnKind::Ghost | BtnKind::Danger => {
            let fill = if pressed || hovered { theme::SURFACE_MUTED } else { Color32::TRANSPARENT };
            let fg = if kind == BtnKind::Danger {
                theme::DANGER
            } else if hovered {
                theme::INK
            } else {
                theme::INK_MUTED
            };
            (fill, Color32::TRANSPARENT, fg)
        }
    };
    let fill = dim(enabled, fill);
    let border = dim(enabled, border);
    let fg = dim(enabled, fg);

    let painter = ui.painter().clone();
    if resp.has_focus() {
        painter.rect_stroke(rect.expand(2.0), theme::R_CONTROL, Stroke::new(2.0, theme::FOCUS), StrokeKind::Outside);
    }
    painter.rect(rect, theme::R_CONTROL, fill, Stroke::new(1.0, border), StrokeKind::Inside);

    let cy = rect.center().y;
    let mut x = rect.center().x - (icon_w + text_w) / 2.0;
    if let Some(name) = icon {
        let icon_rect = Rect::from_center_size(pos2(x + theme::ICON_SM / 2.0, cy), Vec2::splat(theme::ICON_SM));
        cx.icons.paint(&painter, icon_rect, name, fg);
        x += icon_w;
    }
    painter.text(pos2(x, cy), Align2::LEFT_CENTER, label, font, fg);
    contract::mark_widget(&resp, egui::WidgetType::Button, "button", label);

    if enabled {
        resp.on_hover_cursor(CursorIcon::PointingHand)
    } else {
        resp
    }
}

/// A square 36 px button with only a Lucide icon; `label` is the tooltip
/// and the accessible name.
pub fn icon_button(ui: &mut Ui, cx: &Cx, kind: BtnKind, icon_name: &str, label: &str) -> Response {
    let enabled = ui.is_enabled();
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(theme::CONTROL_H), Sense::click());
    let hovered = enabled && resp.hovered();
    let (fill, border, fg) = match kind {
        BtnKind::Primary => {
            let fill = if hovered { theme::SIGNAL.lerp_to_gamma(theme::INK, 0.08) } else { theme::SIGNAL };
            (fill, fill, theme::SIGNAL_INK)
        }
        _ => {
            let fill = if hovered { theme::SURFACE_MUTED } else { theme::SURFACE_RAISED };
            (fill, theme::THREAD_STRONG, theme::INK)
        }
    };
    let painter = ui.painter().clone();
    if resp.has_focus() {
        painter.rect_stroke(rect.expand(2.0), theme::R_CONTROL, Stroke::new(2.0, theme::FOCUS), StrokeKind::Outside);
    }
    painter.rect(rect, theme::R_CONTROL, dim(enabled, fill), Stroke::new(1.0, dim(enabled, border)), StrokeKind::Inside);
    let icon_rect = Rect::from_center_size(rect.center(), Vec2::splat(theme::ICON_SM));
    cx.icons.paint(&painter, icon_rect, icon_name, dim(enabled, fg));
    contract::mark_widget(&resp, egui::WidgetType::Button, "button", label);
    let resp = resp.on_hover_text(label);
    if enabled {
        resp.on_hover_cursor(CursorIcon::PointingHand)
    } else {
        resp
    }
}

/// Like [`button`], but disabled (drawn faint, never clicked).
pub fn button_disabled(ui: &mut Ui, cx: &Cx, kind: BtnKind, size: BtnSize, icon: Option<&str>, label: &str) {
    ui.add_enabled_ui(false, |ui| {
        button(ui, cx, kind, size, icon, label);
    });
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PillKind {
    /// Constructive dot: Online, Hosting, Running.
    Online,
    /// Apricot dot: Connected, live.
    Live,
    /// Hollow dot, faint: Offline, Stopped.
    Off,
    /// Attention: "1 pairing request".
    Warn,
}

/// Width of a status pill with `text`.
pub fn pill_width(ui: &Ui, text: &str) -> f32 {
    9.0 + 6.0 + 6.0 + text_width(ui, text, theme::bold(theme::size::HINT), theme::INK) + 9.0
}

/// Status pill: dot + word. The only round shape in the system.
pub fn pill(ui: &mut Ui, kind: PillKind, text: &str) -> Response {
    let (color, filled) = match kind {
        PillKind::Online => (theme::CONSTRUCTIVE, true),
        PillKind::Live => (theme::SIGNAL, true),
        PillKind::Off => (theme::INK_FAINT, false),
        PillKind::Warn => (theme::ATTENTION, true),
    };
    let font = theme::bold(theme::size::HINT);
    let text_w = text_width(ui, text, font.clone(), color);
    // 9 px inset, 6 px dot, 6 px gap, text, 9 px inset (mockup .status-pill).
    let (rect, resp) = ui.allocate_exact_size(vec2(9.0 + 6.0 + 6.0 + text_w + 9.0, 22.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, theme::R_PILL, theme::SURFACE_MUTED);
    let cy = rect.center().y;
    let dot = pos2(rect.left() + 9.0 + 3.0, cy);
    if filled {
        painter.circle_filled(dot, 3.0, color);
    } else {
        // Hollow ring: 1.5 px stroke, outer radius 3.
        painter.circle_stroke(dot, 2.25, Stroke::new(1.5, color));
    }
    painter.text(pos2(rect.left() + 21.0, cy), Align2::LEFT_CENTER, text, font, color);
    contract::mark_widget(&resp, egui::WidgetType::Label, "pill", text);
    resp
}

/// Width of a [`switch`].
pub const SWITCH_W: f32 = 36.0;

/// On/off switch (moss when on). Returns a response; `.clicked()` means
/// the user asked to flip it. The caller does not mutate `on`; it sends an
/// action and the next snapshot carries the new value.
pub fn switch(ui: &mut Ui, on: bool) -> Response {
    let enabled = ui.is_enabled();
    let (rect, resp) = ui.allocate_exact_size(vec2(SWITCH_W, 20.0), Sense::click());
    let t = ui.ctx().animate_bool_with_time(resp.id, on, theme::T_FAST);

    let track = theme::SURFACE_RAISED.lerp_to_gamma(theme::MOSS, t);
    let edge = theme::THREAD_STRONG.lerp_to_gamma(theme::MOSS, t);
    let knob = theme::INK_FAINT.lerp_to_gamma(theme::INK, t);
    let knob_x = 2.0 + 16.0 * t;
    let knob_rect = Rect::from_min_size(pos2(rect.left() + knob_x, rect.top() + 2.0), Vec2::splat(14.0));

    let painter = ui.painter();
    if resp.has_focus() {
        painter.rect_stroke(rect.expand(2.0), theme::R_FIELD, Stroke::new(2.0, theme::FOCUS), StrokeKind::Outside);
    }
    painter.rect(
        rect,
        R_KEY,
        dim(enabled, track),
        Stroke::new(1.0, dim(enabled, edge)),
        StrokeKind::Inside,
    );
    painter.rect_filled(knob_rect, R_KNOB, dim(enabled, knob));
    contract::mark_widget(&resp, egui::WidgetType::Checkbox, "switch", if on { "on" } else { "off" });

    if enabled {
        resp.on_hover_cursor(CursorIcon::PointingHand)
    } else {
        resp
    }
}

/// Keycaps for a chord, e.g. ["Ctrl", "Alt", "Shift", "Z"], joined by
/// faint "+" signs.
pub fn chord(ui: &mut Ui, keys: &[&str]) {
    const KEY_H: f32 = 20.0;
    let key_font = theme::mono(theme::size::HINT);
    let sep_font = theme::body(theme::size::HINT);
    let key_widths: Vec<f32> = keys
        .iter()
        .map(|k| (text_width(ui, k, key_font.clone(), theme::INK) + 12.0).max(20.0))
        .collect();
    let plus_w = text_width(ui, "+", sep_font.clone(), theme::INK_FAINT);
    let seps = keys.len().saturating_sub(1) as f32;
    let total: f32 = key_widths.iter().sum::<f32>() + seps * (plus_w + 6.0);

    let (rect, resp) = ui.allocate_exact_size(vec2(total, KEY_H), Sense::hover());
    contract::mark_widget(&resp, egui::WidgetType::Label, "chord", &keys.join("+"));
    let painter = ui.painter().clone();
    let cy = rect.center().y;
    let mut x = rect.left();
    for (i, (k, w)) in keys.iter().zip(key_widths).enumerate() {
        if i > 0 {
            x += 3.0;
            painter.text(pos2(x, cy), Align2::LEFT_CENTER, "+", sep_font.clone(), theme::INK_FAINT);
            x += plus_w + 3.0;
        }
        let key = Rect::from_min_size(pos2(x, rect.top()), vec2(w, KEY_H));
        painter.rect(
            key,
            R_KEY,
            theme::SURFACE_MUTED,
            Stroke::new(1.0, theme::THREAD_STRONG),
            StrokeKind::Inside,
        );
        // The 2 px bottom border of kbd.key.
        painter.line_segment(
            [pos2(key.left() + 4.0, key.bottom() - 1.0), pos2(key.right() - 4.0, key.bottom() - 1.0)],
            Stroke::new(2.0, theme::THREAD_STRONG),
        );
        painter.text(key.center(), Align2::CENTER_CENTER, k, key_font.clone(), theme::INK);
        x += w;
    }
}

/// A device or PC ID in Fragment Mono with letter spacing.
pub fn id_text(ui: &mut Ui, id: &str, large: bool) -> Response {
    let (size, color) = if large {
        (theme::size::ID_LG, theme::INK)
    } else {
        (theme::size::ID, theme::INK_MUTED)
    };
    contract::text(ui, spaced(id, theme::mono(size), color, 0.02), false)
}

/// Any monospace evidence text (codes, timers, paths).
pub fn mono_text(ui: &mut Ui, text: &str, size: f32, color: Color32) -> Response {
    ui.label(egui::RichText::new(text).font(theme::mono(size)).color(color))
}

/// A Lucide icon at `size` points tinted `color`.
pub fn icon(ui: &mut Ui, cx: &Cx, name: &str, size: f32, color: Color32) -> Response {
    ui.add(cx.icons.image(name, size, color))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeKind {
    /// Faint info icon, no border colour.
    Neutral,
    /// Information colour.
    Info,
    /// Danger seam (refused key, PC key changed).
    Error,
}

/// A notice block: icon on the left, contents on the right, fitted field
/// radius, a 3 px seam on the left in the kind's colour.
pub fn notice(ui: &mut Ui, cx: &Cx, kind: NoticeKind, icon_name: &str, add: impl FnOnce(&mut Ui)) -> Response {
    let (seam, icon_color, border) = match kind {
        NoticeKind::Neutral => (theme::THREAD_STRONG, theme::INK_FAINT, theme::THREAD),
        NoticeKind::Info => (theme::INFORMATION, theme::INFORMATION, theme::THREAD),
        NoticeKind::Error => (theme::DANGER, theme::DANGER, theme::DANGER.gamma_multiply(0.45)),
    };
    // Padding 12 / 14 / 12 / 12 from the mockup, plus the 3 px seam on the left.
    let frame = egui::Frame::NONE
        .fill(theme::SURFACE_RAISED)
        .stroke(Stroke::new(1.0, border))
        .corner_radius(theme::R_FIELD)
        .inner_margin(Margin { left: 15, right: 14, top: 12, bottom: 12 });
    let out = contract::framed(ui, "notice", frame, |ui| {
            ui.set_width(ui.available_width());
            top_row(ui, |ui| {
                icon(ui, cx, icon_name, theme::ICON, icon_color);
                ui.add_space(theme::SP2 + 2.0);
                ui.vertical(|ui| {
                    ui.style_mut().visuals.override_text_color = Some(theme::INK_MUTED);
                    add(ui);
                });
            });
        });
    let r = out.response.rect;
    let seam_rect = Rect::from_min_max(r.min, pos2(r.left() + 3.0, r.bottom()));
    let field = theme::R_FIELD;
    ui.painter().rect_filled(
        seam_rect,
        CornerRadius { nw: field.nw, ne: 0, se: 0, sw: field.sw },
        seam,
    );
    out.response
}

/// Width of a [`tag`] with `text`.
pub fn tag_width(ui: &Ui, text: &str) -> f32 {
    text_width(ui, text, theme::bold(10.5), theme::INK) + 12.0
}

/// Small bordered tag, e.g. "vouched by Pixel 8"; `draft` uses the
/// attention colour for "Draft" labels.
pub fn tag(ui: &mut Ui, text: &str, draft: bool) -> Response {
    let (fg, border) = if draft {
        (theme::ATTENTION, theme::ATTENTION.gamma_multiply(0.45))
    } else {
        (theme::INK_FAINT, theme::THREAD_STRONG)
    };
    let galley = ui.painter().layout_no_wrap(text.to_owned(), theme::bold(10.5), fg);
    // Padding 3 px / 5 px plus 1 px border on each side.
    let size = vec2(galley.size().x + 12.0, galley.size().y + 8.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    let painter = ui.painter();
    painter.rect(rect, theme::R_COMPACT, Color32::TRANSPARENT, Stroke::new(1.0, border), StrokeKind::Inside);
    painter.galley(rect.min + vec2(6.0, 4.0), galley, fg);
    contract::mark_widget(&resp, egui::WidgetType::Label, "tag", text);
    resp
}

/// Section label (12 px bold, faint), like the mockups' h3 / eyebrow.
pub fn section_label(ui: &mut Ui, text: &str) -> Response {
    ui.label(egui::RichText::new(text).font(theme::bold(theme::size::SMALL)).color(theme::INK_FAINT))
}

/// A read-only input box look (label above, value inside), used where the
/// mockups show a filled or focused field. `focused` draws the focus ring.
pub fn field(ui: &mut Ui, cx: &Cx, label: &str, icon_name: Option<&str>, focused: bool, add: impl FnOnce(&mut Ui)) -> Response {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 6.0;
        ui.label(egui::RichText::new(label).font(theme::bold(theme::size::SMALL)).color(theme::INK_MUTED));
        let border = if focused { theme::INK_FAINT } else { theme::THREAD_STRONG };
        let frame = egui::Frame::NONE
            .fill(theme::SURFACE_RAISED)
            .stroke(Stroke::new(1.0, border))
            .corner_radius(theme::R_FIELD)
            .inner_margin(Margin::symmetric(12, 0));
        let out = contract::framed(ui, "field", frame, |ui| {
                // One line of FIELD_H - 2 (inside the 1 px border). A row of
                // exactly that height; `horizontal_centered` would claim all
                // the remaining height of the pane.
                let h = theme::FIELD_H - 2.0;
                ui.allocate_ui_with_layout(vec2(ui.available_width(), h), Layout::left_to_right(Align::Center), |ui| {
                    ui.set_min_height(h);
                    if let Some(name) = icon_name {
                        icon(ui, cx, name, theme::ICON, theme::INK_FAINT);
                        ui.add_space(theme::SP2 + 2.0);
                    }
                    add(ui);
                });
            });
        let rect = out.response.rect;
        if focused {
            // 2 px ring, 1 px outside the border (outline 2 px, offset 1 px).
            ui.painter().rect_stroke(rect.expand(1.0), theme::R_FIELD, Stroke::new(2.0, theme::FOCUS), StrokeKind::Outside);
        }
        out.response
    })
    .inner
}

/// The step list on the flow rail: done (check), now (apricot square),
/// waiting (hollow), failed (danger). Each item: bold title + faint sub.
pub fn rail_steps(ui: &mut Ui, cx: &Cx, steps: &[(&str, &str, StepStatus)]) {
    let _ = cx;
    let count = steps.len();
    for (i, (title, sub, status)) in steps.iter().enumerate() {
        let title_color = match status {
            StepStatus::Now | StepStatus::Failed => theme::INK,
            StepStatus::Done | StepStatus::Waiting => theme::INK_MUTED,
        };
        // 30 px indent for the marker column, then title over sub.
        let row = top_row(ui, |ui| {
                ui.add_space(30.0);
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new(*title).font(theme::bold(13.0)).color(title_color));
                    ui.label(egui::RichText::new(*sub).font(theme::body(13.0)).color(theme::INK_FAINT));
                });
            })
            .rect;

        let painter = ui.painter().clone();
        let (fill, edge) = match status {
            StepStatus::Done => (theme::MOSS, theme::MOSS),
            StepStatus::Now => (theme::SIGNAL, theme::SIGNAL),
            StepStatus::Failed => (theme::DANGER, theme::DANGER),
            StepStatus::Waiting => (theme::VOID, theme::THREAD_STRONG),
        };
        let marker = Rect::from_min_size(pos2(row.left() + 6.0, row.top() + 4.0), Vec2::splat(9.0));
        painter.rect(marker, R_DOT, fill, Stroke::new(1.5, edge), StrokeKind::Inside);
        if i + 1 < count {
            // Connector from under the marker to just above the next one.
            let x = row.left() + 11.0;
            painter.line_segment(
                [pos2(x, row.top() + 17.0), pos2(x, row.bottom() + 16.0)],
                Stroke::new(1.0, theme::THREAD),
            );
        }
        // 18 px item padding below each step.
        ui.add_space(18.0);
    }
}

/// The connection checklist in flow panes: icon per status, label, and
/// right-aligned mono detail. `Now` spins (request a repaint at 30 Hz
/// while visible).
pub fn conn_steps(ui: &mut Ui, cx: &Cx, steps: &[ConnStep]) {
    if steps.iter().any(|s| s.status == StepStatus::Now) {
        ui.ctx().request_repaint_after(Duration::from_millis(33));
    }
    let angle = ui.input(|i| i.time) as f32 * TAU / 1.1;

    for (i, s) in steps.iter().enumerate() {
        if i > 0 {
            ui.add_space(2.0);
        }
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 40.0), Sense::hover());
        contract::row_at(ui, "conn-step", rect, |ui| {
        let (icon_name, icon_color, label_color, label_font, bg) = match s.status {
            StepStatus::Done => ("circle-check", theme::CONSTRUCTIVE, theme::INK_MUTED, theme::body(14.0), None),
            StepStatus::Now => ("loader-circle", theme::SIGNAL, theme::INK, theme::bold(14.0), Some(theme::SURFACE)),
            StepStatus::Failed => ("circle-alert", theme::DANGER, theme::INK, theme::bold(14.0), Some(theme::SURFACE)),
            StepStatus::Waiting => ("clock", theme::INK_FAINT, theme::INK_FAINT, theme::body(14.0), None),
        };

        let painter = ui.painter().clone();
        if let Some(bg) = bg {
            painter.rect_filled(rect, theme::R_FIELD, bg);
        }
        let cy = rect.center().y;
        // Icon column: 22 px at the 12 px inset, icon 18 px centred in it.
        let icon_rect = Rect::from_center_size(pos2(rect.left() + 12.0 + STEP_COL / 2.0, cy), Vec2::splat(theme::ICON));
        if s.status == StepStatus::Now {
            cx.icons
                .image(icon_name, theme::ICON, icon_color)
                .rotate(angle, vec2(0.5, 0.5))
                .paint_at(ui, icon_rect);
        } else {
            cx.icons.paint(&painter, icon_rect, icon_name, icon_color);
        }
        contract::paint_text(
            ui,
            pos2(rect.left() + 12.0 + STEP_COL + 12.0, cy),
            Align2::LEFT_CENTER,
            &s.label,
            label_font,
            label_color,
        );
        contract::paint_text(
            ui,
            pos2(rect.right() - 12.0, cy),
            Align2::RIGHT_CENTER,
            &s.detail,
            theme::mono(theme::size::HINT),
            theme::INK_FAINT,
        );
        });
    }
}

/// The 6-digit comparison number, grouped 3+3 ("482 913").
pub fn number(ui: &mut Ui, n: u32, size: f32) -> Response {
    let s = format!("{:03} {:03}", n / 1000, n % 1000);
    ui.label(spaced(&s, theme::mono(size), theme::INK, 0.06))
}

/// A thin horizontal divider in THREAD colour.
pub fn divider(ui: &mut Ui) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::ZERO, theme::THREAD);
}

// ------------------------------------------------------------ helpers

/// A row that sizes to its content, with children top-aligned. Same height
/// rule as egui's `horizontal` (one interact height as the start, grown by
/// content). egui's `horizontal_top` starts from all the remaining height, so
/// everything after it in a pane was pushed out of view.
fn top_row(ui: &mut Ui, add: impl FnOnce(&mut Ui)) -> Response {
    let h = ui.spacing().interact_size.y;
    ui.allocate_ui_with_layout(vec2(ui.available_width(), h), Layout::left_to_right(Align::Min), add).response
}

/// Colour with the disabled opacity applied when `enabled` is false.
pub fn dim(enabled: bool, c: Color32) -> Color32 {
    if enabled {
        c
    } else {
        c.gamma_multiply(DISABLED_ALPHA)
    }
}

/// Width of one line of text in the given font and colour.
fn text_width(ui: &Ui, text: &str, font: FontId, color: Color32) -> f32 {
    ui.painter().layout_no_wrap(text.to_owned(), font, color).size().x
}

/// Text with letter spacing in em (CSS `letter-spacing`), as a label.
fn spaced(text: &str, font: FontId, color: Color32, em: f32) -> WidgetText {
    let extra_letter_spacing = font.size * em;
    let format = TextFormat { font_id: font, color, extra_letter_spacing, ..Default::default() };
    LayoutJob::single_section(text.to_owned(), format).into()
}
