//! Session chrome over the video: collapsed tab, toolbar strip, monitor
//! picker, stats panel, capture/released toasts, reconnecting card, Admin
//! required banner, secure desktop badge. Mockup:
//! client-win/mockups/session.html.
//!
//! Every piece is an `egui::Area` on `Order::Foreground`, attached to the top
//! edge (or centred). The video is the shell's layer under all of it; the
//! reconnecting dim is a translucent VOID wash over the same rect (no blur).
//! Buttons go into `cx.out.no_drag`, the rest of the strip into `cx.out.drag`.

use std::time::Duration;

use egui::{
    pos2, vec2, Align, Align2, Color32, CornerRadius, Frame, Id, Layout, Margin, Order, Painter,
    Pos2, Rect, Response, RichText, Sense, Stroke, StrokeKind, Ui,
};

use super::contract;
use super::icons::Icons;
use super::theme;
use super::widgets::{self, dim, BtnKind, BtnSize, NoticeKind};
use super::{Cx, StripView, Toast, UiState};
use crate::model::{Action, AppState, Link, Quality, Session, SessionNotice, StreamMode};

/// Mockup `.tb-btn` and `.cap` corners (5 8 4 7).
const SEAM_R: CornerRadius = CornerRadius { nw: 5, ne: 8, se: 4, sw: 7 };
/// Mockup `.handle` corners: square top, 8 10 at the bottom.
const TAB_R: CornerRadius = CornerRadius { nw: 0, ne: 0, se: 8, sw: 10 };
/// Collapsed tab opacity (mockup `.handle`).
const TAB_OPACITY: f32 = 0.82;
/// Gap between the stats panel and the window's right edge.
const STATS_MARGIN: f32 = 12.0;
/// Mockup `.banner` corners.
const BANNER_R: CornerRadius = CornerRadius { nw: 0, ne: 0, se: 12, sw: 14 };
/// Mockup `.banner` width.
const BANNER_W: f32 = 620.0;
/// Room kept for the Dismiss button on the banner's right.
const BANNER_DISMISS_SLOT: f32 = 110.0;
/// Mockup `.toast .ic` and `.layout` corners.
const ICON_BOX_R: CornerRadius = CornerRadius { nw: 6, ne: 9, se: 6, sw: 8 };

const BTN_H: f32 = 32.0;
/// Round trip at or below this reads as good (CONSTRUCTIVE). Judgement value.
const GOOD_RTT_MS: u32 = 30;
/// The sparkline's top edge; the 16.7 ms line sits at mid height.
const FRAME_SCALE_MS: f32 = 33.4;
const SPARK_N: usize = 240;
/// Popovers drop this far below the strip.
const POP_GAP: f32 = 6.0;
/// Toast top when only the tab is showing (mockup `top: 40px`).
const TOAST_TOP_TAB: f32 = 40.0;
/// Reconnecting dim over the frozen frame (mockup brightness .45 as a wash).
const DIM_ALPHA: u8 = 140;
const PICKER_W: f32 = 340.0;
/// Inner width of the picker (panel 340 less 14 padding each side).
const PICKER_INNER_W: f32 = 312.0;
const MAP_H: f32 = 128.0;
const NUM_KEYS: [egui::Key; 9] = [
    egui::Key::Num1,
    egui::Key::Num2,
    egui::Key::Num3,
    egui::Key::Num4,
    egui::Key::Num5,
    egui::Key::Num6,
    egui::Key::Num7,
    egui::Key::Num8,
    egui::Key::Num9,
];

/// Requests collected while the chrome is drawn, applied after the frame.
#[derive(Default)]
struct Picks {
    acts: Vec<Action>,
    drag: Vec<Rect>,
    no_drag: Vec<Rect>,
    strip: Option<StripView>,
    toggle_stats: bool,
    dismiss: bool,
    /// Where the Display button sits this frame; the picker drops from it.
    display: Option<Rect>,
}

/// A fill colour with the given alpha (the mockup's rgba() tints).
fn tint(c: Color32, a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), a)
}

pub fn show(ctx: &egui::Context, cx: &mut Cx, state: &AppState, sess: &Session, us: &mut UiState) {
    let _ = state;
    let screen = ctx.content_rect();
    cx.out.video = Some(screen);

    let reconnecting = matches!(sess.link, Link::Reconnecting { .. });
    let secure = sess.notice == Some(SessionNotice::SecureDesktop);
    let admin = sess.notice == Some(SessionNotice::AdminRequired);
    let view = us.session.strip;
    let picker = view == StripView::MonitorPicker;
    let open = view != StripView::Collapsed;
    let chrome_h = if open { theme::STRIP_H } else { theme::TAB_H };
    let stats_open = us.session.stats_open;

    let dismiss_id = Id::new("session.admin.dismissed");
    let dismissed = ctx.data(|d| d.get_temp::<bool>(dismiss_id).unwrap_or(false));
    if !admin && dismissed {
        ctx.data_mut(|d| d.insert_temp(dismiss_id, false));
    }

    // Keys while the picker is open: 1-9 choose a display, Esc closes it.
    let (num, esc) = if picker {
        ctx.input(|i| {
            (
                NUM_KEYS.iter().position(|k| i.key_pressed(*k)),
                i.key_pressed(egui::Key::Escape),
            )
        })
    } else {
        (None, false)
    };

    // Dim first, so every chrome Area created after it sits above it.
    if reconnecting {
        egui::Area::new(Id::new("session.dim"))
            .order(Order::Foreground)
            .fixed_pos(screen.min)
            .interactable(false)
            .show(ctx, |ui| {
                ui.painter().rect_filled(screen, CornerRadius::ZERO, tint(theme::VOID, DIM_ALPHA));
            });
    }

    let mut o = Picks::default();
    {
        let cxr: &Cx = &*cx;
        if open && secure {
            secure_strip(ctx, cxr, sess, view, stats_open, &mut o);
        } else if open {
            toolbar(ctx, cxr, sess, view, stats_open, &mut o);
        } else {
            tab(ctx, cxr, sess, secure, reconnecting, &mut o);
        }
        if picker {
            picker_panel(ctx, cxr, sess, screen, o.display, &mut o);
        }
        if stats_open {
            stats_panel(ctx, sess, chrome_h);
        }
        if let Some(t) = us.session.toast {
            let top = if open { theme::STRIP_H + POP_GAP } else { TOAST_TOP_TAB };
            toast(ctx, cxr, sess, t, top);
        }
        if admin && !dismissed {
            banner(ctx, cxr, chrome_h, &mut o);
        }
        if reconnecting {
            recovery(ctx, cxr, sess, &mut o);
        }
    }

    let Picks { mut acts, drag, no_drag, mut strip, toggle_stats, dismiss, .. } = o;
    if picker {
        if let Some(n) = num {
            // While a switch is pending the number keys do nothing; Esc still closes.
            if n < sess.monitors.len() && !sess.monitor_pending {
                acts.push(Action::SetMonitor(n));
                strip = Some(StripView::Toolbar);
            }
        } else if esc {
            strip = Some(StripView::Toolbar);
        }
    }
    if dismiss {
        ctx.data_mut(|d| d.insert_temp(dismiss_id, true));
    }
    if toggle_stats {
        us.session.stats_open = !us.session.stats_open;
    }
    if let Some(s) = strip {
        us.session.strip = s;
    }
    cx.out.drag.extend(drag);
    cx.out.no_drag.extend(no_drag);
    cx.actions.extend(acts);
}

// ------------------------------------------------------------ small pieces

fn label(ui: &mut Ui, text: &str, font: egui::FontId, color: Color32) {
    ui.label(RichText::new(text).font(font).color(color));
}

fn attempt_of(sess: &Session) -> u32 {
    match sess.link {
        Link::Reconnecting { attempt, .. } => attempt,
        Link::Live => 0,
    }
}

/// Live dot (7 px, 3 px ring) with the mockup's apricot or attention colour.
fn live_dot(ui: &mut Ui, color: Color32, ring: Color32) {
    let (r, _) = ui.allocate_exact_size(vec2(13.0, 13.0), Sense::hover());
    let p = ui.painter();
    p.circle_filled(r.center(), 6.5, ring);
    p.circle_filled(r.center(), 3.5, color);
}

/// Connection chip: live dot, name, mono evidence. `warn` is the reconnecting dot.
fn chip(ui: &mut Ui, warn: bool, name: &str, evidence: &str, compact: bool) {
    let (dot, ring) = if warn {
        (theme::ATTENTION, tint(theme::ATTENTION, 46))
    } else {
        (theme::SIGNAL, tint(theme::SIGNAL, 46))
    };
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        live_dot(ui, dot, ring);
        label(ui, name, theme::bold(12.5), theme::INK);
        if !compact {
            label(ui, evidence, theme::mono(11.5), theme::INK_MUTED);
        }
    })
    .response
    .on_hover_text(evidence);
}

/// Seam between strip groups: 1 px by 22 px, 5 px either side.
fn seam(ui: &mut Ui) {
    ui.add_space(5.0);
    let (r, _) = ui.allocate_exact_size(vec2(1.0, 22.0), Sense::hover());
    ui.painter().rect_filled(r, CornerRadius::ZERO, theme::THREAD_STRONG);
    ui.add_space(5.0);
}

/// Border on chosen sides only (the chrome sits on the top edge, so no top line).
fn seams(p: &Painter, r: Rect, top: bool, right: bool, bottom: bool, left: bool) {
    let s = Stroke::new(1.0, theme::THREAD_STRONG);
    if top {
        p.line_segment([r.left_top(), r.right_top()], s);
    }
    if right {
        p.line_segment([r.right_top(), r.right_bottom()], s);
    }
    if bottom {
        p.line_segment([r.left_bottom(), r.right_bottom()], s);
    }
    if left {
        p.line_segment([r.left_top(), r.left_bottom()], s);
    }
}

fn strip_frame() -> Frame {
    Frame::NONE
        .fill(theme::CHROME_BG)
        .corner_radius(theme::R_STRIP)
        .shadow(theme::CHROME_SHADOW)
        .inner_margin(Margin { left: 5, right: 5, top: 0, bottom: 0 })
}

/// Keeps a button's rect clickable inside the draggable strip.
fn keep(o: &mut Picks, r: Response) -> Response {
    o.no_drag.push(r.rect);
    r
}

// ------------------------------------------------------------ toolbar buttons

struct Btn<'a> {
    icon: Option<&'static str>,
    label: Option<&'a str>,
    chev: Option<&'static str>,
    /// Open or selected: ink fill, ink-on-apricot text (mockup `.tb-btn.open`).
    open: bool,
    /// Leave: danger tint on hover.
    leave: bool,
}

/// Mockup `.tb-btn`: 32 px, fitted corners, muted text that lifts on hover.
/// Icon-only buttons are 34 px wide.
fn tb_btn(ui: &mut Ui, icons: &Icons, b: Btn, tip: &str) -> Response {
    tb_btn_enabled(ui, icons, b, tip, true)
}

/// [`tb_btn`], or when `enabled` is false drawn dimmed with no hover lift and
/// never clicked. Same size and place either way; the tooltip still shows.
fn tb_btn_enabled(ui: &mut Ui, icons: &Icons, b: Btn, tip: &str, enabled: bool) -> Response {
    let font = theme::bold(12.5);
    let label_w = match b.label {
        Some(t) => ui.painter().layout_no_wrap(t.to_owned(), font.clone(), theme::INK).size().x,
        None => 0.0,
    };
    let icon_w = if b.icon.is_some() { 14.0 } else { 0.0 };
    let chev_w = if b.chev.is_some() { 13.0 } else { 0.0 };
    let parts = [b.icon.is_some(), b.label.is_some(), b.chev.is_some()]
        .iter()
        .filter(|x| **x)
        .count();
    let gaps = parts.saturating_sub(1) as f32 * 7.0;
    let icon_only = b.label.is_none() && b.chev.is_none();
    let w = if icon_only { 34.0 } else { 20.0 + icon_w + label_w + chev_w + gaps };

    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(vec2(w, BTN_H), sense);
    let hot = enabled && resp.hovered();
    let (bg, fg) = if b.open {
        (theme::INK, theme::SIGNAL_INK)
    } else if b.leave && hot {
        (tint(theme::DANGER, 36), theme::DANGER)
    } else if hot {
        (theme::SURFACE_MUTED, theme::INK)
    } else {
        (Color32::TRANSPARENT, theme::INK_MUTED)
    };
    let fg = dim(enabled, fg);

    let p = ui.painter().clone();
    if bg != Color32::TRANSPARENT {
        p.rect_filled(rect, SEAM_R, bg);
    }
    let cy = rect.center().y;
    let mut x = if icon_only { rect.center().x - 7.0 } else { rect.left() + 10.0 };
    if let Some(name) = b.icon {
        icons.paint(&p, Rect::from_center_size(pos2(x + 7.0, cy), vec2(14.0, 14.0)), name, fg);
        x += icon_w + 7.0;
    }
    if let Some(t) = b.label {
        p.text(pos2(x, cy), Align2::LEFT_CENTER, t, font, fg);
        x += label_w + 7.0;
    }
    if let Some(name) = b.chev {
        icons.paint(&p, Rect::from_center_size(pos2(x + 6.5, cy), vec2(13.0, 13.0)), name, fg.linear_multiply(0.7));
    }
    contract::mark_widget(&resp, egui::WidgetType::Button, "button", b.label.unwrap_or(if tip.is_empty() { "button" } else { tip }));
    if tip.is_empty() {
        resp
    } else {
        resp.on_hover_text(tip)
    }
}

/// Display button: opens the monitor picker, chevron flips when open.
fn display_btn(ui: &mut Ui, cx: &Cx, sess: &Session, picker: bool, o: &mut Picks) {
    let text = format!("Display {}", sess.monitor + 1);
    let r = keep(
        o,
        tb_btn(
            ui,
            cx.icons,
            Btn {
                icon: Some("monitor"),
                label: Some(text.as_str()),
                chev: Some(if picker { "chevron-up" } else { "chevron-down" }),
                open: picker,
                leave: false,
            },
            "Choose the display to show",
        ),
    );
    o.display = Some(r.rect);
    if r.clicked() {
        o.strip = Some(if picker { StripView::Toolbar } else { StripView::MonitorPicker });
    }
}

fn stats_btn(ui: &mut Ui, icons: &Icons, open: bool, o: &mut Picks) {
    let r = keep(
        o,
        tb_btn(
            ui,
            icons,
            Btn { icon: Some("activity"), label: Some("Stats"), chev: None, open, leave: false },
            "Stream stats (Ctrl+Alt+Shift+S)",
        ),
    );
    if r.clicked() {
        o.toggle_stats = true;
    }
}

fn leave_btn(ui: &mut Ui, icons: &Icons, o: &mut Picks) {
    let r = keep(
        o,
        tb_btn(
            ui,
            icons,
            Btn { icon: Some("log-out"), label: Some("Leave"), chev: None, open: false, leave: true },
            "",
        ),
    );
    if r.clicked() {
        o.acts.push(Action::Disconnect);
    }
}

/// Capture group states. Blocked and Paused replace the capture chord.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Cap {
    Captured,
    Blocked,
    Paused,
    Released,
}

/// Below this window width the strip goes compact: no release chord in the
/// capture group and no word on the quality button, so the strip still fits
/// the 760 px minimum window.
const STRIP_COMPACT_BELOW: f32 = 1000.0;

fn cap(ui: &mut Ui, cx: &Cx, kind: Cap, compact: bool) {
    let (icon, text, fg, bg, keys) = match kind {
        Cap::Captured => ("mouse-pointer-2", "Captured", theme::SIGNAL, tint(theme::SIGNAL, 31), true),
        Cap::Blocked => ("shield-alert", "Input blocked", theme::ATTENTION, tint(theme::ATTENTION, 31), false),
        Cap::Paused => ("pause", "Input paused", theme::INK_MUTED, Color32::TRANSPARENT, false),
        Cap::Released => ("mouse-pointer-2", "Released", theme::INK_MUTED, Color32::TRANSPARENT, false),
    };
    let mut f = Frame::NONE
        .fill(bg)
        .corner_radius(SEAM_R)
        .inner_margin(Margin { left: 9, right: 8, top: 0, bottom: 0 });
    if matches!(kind, Cap::Paused | Cap::Released) {
        f = f.stroke(Stroke::new(1.0, theme::THREAD_STRONG));
    }
    f.show(ui, |ui| {
        ui.set_min_height(BTN_H);
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            widgets::icon(ui, cx, icon, 14.0, fg).on_hover_text(text);
            // Compact keeps the word only for the normal Captured state; the
            // others show their icon, with the word on hover.
            if !compact || kind == Cap::Captured {
                label(ui, text, theme::bold(12.5), fg);
            }
            if keys && !compact {
                widgets::chord(ui, &["Ctrl", "Alt", "Shift", "Z"]);
            }
        });
    });
}

/// Mockup `.badge-secure`: attention pill with the secure desktop copy.
fn secure_badge(ui: &mut Ui, cx: &Cx) {
    Frame::NONE
        .fill(tint(theme::ATTENTION, 36))
        .corner_radius(theme::R_PILL)
        .inner_margin(Margin { left: 10, right: 10, top: 0, bottom: 0 })
        .show(ui, |ui| {
            ui.set_min_height(26.0);
            ui.horizontal_centered(|ui| {
                ui.spacing_mut().item_spacing.x = 7.0;
                widgets::icon(ui, cx, "shield-alert", 14.0, theme::ATTENTION);
                label(ui, "Windows secure desktop · protected input", theme::bold(12.0), theme::ATTENTION);
            });
        });
}

// ------------------------------------------------------------ strips

/// Expanded strip: connection, capture, stream controls, then Leave.
fn toolbar(ctx: &egui::Context, cx: &Cx, sess: &Session, view: StripView, stats_open: bool, o: &mut Picks) {
    let reconnecting = matches!(sess.link, Link::Reconnecting { .. });
    let admin = sess.notice == Some(SessionNotice::AdminRequired);
    let picker = view == StripView::MonitorPicker;
    let compact = ctx.content_rect().width() < STRIP_COMPACT_BELOW;
    egui::Area::new(Id::new("session.strip"))
        .order(Order::Foreground)
        .anchor(Align2::CENTER_TOP, vec2(0.0, 0.0))
        .show(ctx, |ui| {
            let f = contract::framed(ui, "strip", strip_frame(), |ui| {
                ui.set_min_height(theme::STRIP_H);
                ui.horizontal_centered(|ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    if reconnecting {
                        chip(ui, true, "Reconnecting", &format!("attempt {}", attempt_of(sess)), compact);
                    } else {
                        chip(ui, false, &sess.pc_name, &format!("{} ms", sess.rtt_ms), compact);
                    }
                    seam(ui);
                    if reconnecting {
                        cap(ui, cx, Cap::Paused, compact);
                    } else if admin {
                        cap(ui, cx, Cap::Blocked, compact);
                    } else if sess.captured {
                        cap(ui, cx, Cap::Captured, compact);
                    } else {
                        cap(ui, cx, Cap::Released, compact);
                    }
                    seam(ui);
                    display_btn(ui, cx, sess, picker, o);
                    // Mode: one button showing the current mode (mockup
                    // `Game` with chevrons-up-down); a click switches to the other.
                    let (mtext, micon, mnext) = match sess.mode {
                        StreamMode::Work => ("Work", "briefcase", StreamMode::Game),
                        StreamMode::Game => ("Game", "gamepad-2", StreamMode::Work),
                    };
                    let m = keep(
                        o,
                        tb_btn(
                            ui,
                            cx.icons,
                            Btn {
                                icon: Some(micon),
                                label: if compact { None } else { Some(mtext) },
                                chev: Some("chevrons-up-down"),
                                open: false,
                                leave: false,
                            },
                            "Stream mode: click to switch",
                        ),
                    );
                    if m.clicked() {
                        o.acts.push(Action::SetMode(mnext));
                    }
                    // Quality cycles Auto, Sharp, Smooth (no menu in the mockup).
                    let (qtext, qnext) = match sess.quality {
                        Quality::Auto => ("Auto", Quality::Sharp),
                        Quality::Sharp => ("Sharp", Quality::Smooth),
                        Quality::Smooth => ("Smooth", Quality::Auto),
                    };
                    let q = keep(
                        o,
                        tb_btn_enabled(
                            ui,
                            cx.icons,
                            Btn {
                                icon: Some("gauge"),
                                label: if compact { None } else { Some(qtext) },
                                chev: if compact { None } else { Some("chevron-down") },
                                open: false,
                                leave: false,
                            },
                            if sess.quality_controls_available {
                                "Picture quality: click to change"
                            } else {
                                "Quality: not available yet"
                            },
                            sess.quality_controls_available,
                        ),
                    );
                    if q.clicked() {
                        o.acts.push(Action::SetQuality(qnext));
                    }
                    let (audio_icon, audio_tip) = if !sess.audio_controls_available {
                        (if sess.audio { "volume-2" } else { "volume-x" }, "Audio: not available yet")
                    } else if sess.audio {
                        ("volume-2", "Audio on")
                    } else {
                        ("volume-x", "Audio off")
                    };
                    let a = keep(
                        o,
                        tb_btn_enabled(
                            ui,
                            cx.icons,
                            Btn { icon: Some(audio_icon), label: None, chev: None, open: false, leave: false },
                            audio_tip,
                            sess.audio_controls_available,
                        ),
                    );
                    if a.clicked() {
                        o.acts.push(Action::SetAudio(!sess.audio));
                    }
                    stats_btn(ui, cx.icons, stats_open, o);
                    // Settings has no action in the session yet: drawn, not wired.
                    let _ = keep(
                        o,
                        tb_btn(
                            ui,
                            cx.icons,
                            Btn { icon: Some("settings"), label: None, chev: None, open: false, leave: false },
                            "Settings",
                        ),
                    );
                    seam(ui);
                    leave_btn(ui, cx.icons, o);
                    let c = keep(
                        o,
                        tb_btn(
                            ui,
                            cx.icons,
                            Btn { icon: Some("chevron-up"), label: None, chev: None, open: false, leave: false },
                            "Collapse",
                        ),
                    );
                    if c.clicked() {
                        o.strip = Some(StripView::Collapsed);
                    }
                });
            });
            let r = f.response.rect;
            seams(ui.painter(), r, false, true, true, true);
            o.drag.push(r);
        });
}

/// Secure desktop: the strip stays open, with the badge in place of capture.
fn secure_strip(ctx: &egui::Context, cx: &Cx, sess: &Session, view: StripView, stats_open: bool, o: &mut Picks) {
    egui::Area::new(Id::new("session.strip"))
        .order(Order::Foreground)
        .anchor(Align2::CENTER_TOP, vec2(0.0, 0.0))
        .show(ctx, |ui| {
            let f = contract::framed(ui, "strip", strip_frame(), |ui| {
                ui.set_min_height(theme::STRIP_H);
                ui.horizontal_centered(|ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    chip(ui, false, &sess.pc_name, &format!("{} ms", sess.rtt_ms), false);
                    seam(ui);
                    secure_badge(ui, cx);
                    seam(ui);
                    display_btn(ui, cx, sess, view == StripView::MonitorPicker, o);
                    stats_btn(ui, cx.icons, stats_open, o);
                    seam(ui);
                    leave_btn(ui, cx.icons, o);
                });
            });
            let r = f.response.rect;
            seams(ui.painter(), r, false, true, true, true);
            o.drag.push(r);
        });
}

/// Collapsed: a 26 px tab on the top edge. Click opens the toolbar.
fn tab(ctx: &egui::Context, cx: &Cx, sess: &Session, secure: bool, reconnecting: bool, o: &mut Picks) {
    let clicked = egui::Area::new(Id::new("session.tab"))
        .order(Order::Foreground)
        .anchor(Align2::CENTER_TOP, vec2(0.0, 0.0))
        .show(ctx, |ui| {
            // Mockup `.handle { opacity: .82 }`: the resting tab sits slightly faded.
            ui.multiply_opacity(TAB_OPACITY);
            let f = Frame::NONE
                .fill(theme::CHROME_BG)
                .corner_radius(TAB_R)
                .shadow(theme::CHROME_SHADOW)
                .inner_margin(Margin { left: 10, right: 12, top: 0, bottom: 0 })
                .show(ui, |ui| {
                    ui.set_min_height(theme::TAB_H);
                    ui.horizontal_centered(|ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        if secure {
                            live_dot(ui, theme::ATTENTION, tint(theme::ATTENTION, 46));
                            label(ui, "Secure desktop", theme::bold(12.0), theme::ATTENTION);
                        } else if reconnecting {
                            live_dot(ui, theme::ATTENTION, tint(theme::ATTENTION, 46));
                            label(ui, "Reconnecting", theme::bold(12.0), theme::INK_MUTED);
                            label(ui, &format!("attempt {}", attempt_of(sess)), theme::mono(11.0), theme::INK);
                        } else {
                            live_dot(ui, theme::SIGNAL, tint(theme::SIGNAL, 46));
                            label(ui, &sess.pc_name, theme::bold(12.0), theme::INK_MUTED);
                            label(ui, &format!("{} ms", sess.rtt_ms), theme::mono(11.0), theme::INK);
                        }
                        widgets::icon(ui, cx, "chevron-down", 14.0, theme::INK_MUTED);
                    });
                });
            let r = f.response.rect;
            seams(ui.painter(), r, false, true, true, true);
            ui.interact(r, Id::new("session.tab.hit"), Sense::click())
        })
        .inner;
    if clicked.clicked() {
        o.strip = Some(StripView::Toolbar);
    }
}

// ------------------------------------------------------------ monitor picker

fn picker_panel(ctx: &egui::Context, cx: &Cx, sess: &Session, screen: Rect, display: Option<Rect>, o: &mut Picks) {
    let centre = display.map(|r| r.center().x).unwrap_or(screen.center().x);
    let max_x = (screen.right() - PICKER_W - 8.0).max(screen.left() + 8.0);
    let x = (centre - PICKER_W / 2.0).clamp(screen.left() + 8.0, max_x);
    egui::Area::new(Id::new("session.monitors"))
        .order(Order::Foreground)
        .fixed_pos(pos2(x, theme::STRIP_H + POP_GAP))
        .show(ctx, |ui| {
            Frame::NONE
                .fill(theme::SURFACE)
                .stroke(Stroke::new(1.0, theme::THREAD_STRONG))
                .corner_radius(theme::R_PANEL)
                .shadow(theme::CHROME_SHADOW)
                .inner_margin(Margin::same(14))
                .show(ui, |ui| {
                    ui.set_width(PICKER_W - 28.0);
                    ui.horizontal(|ui| {
                        label(ui, "Show display", theme::bold(12.0), theme::INK_FAINT);
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            label(
                                ui,
                                &format!("{} on {}", sess.monitors.len(), sess.pc_name),
                                theme::mono(10.5),
                                theme::INK_FAINT,
                            );
                        });
                    });
                    ui.add_space(10.0);
                    monitor_map(ui, sess, o);
                    ui.add_space(10.0);
                    monitor_rows(ui, cx, sess, o);
                    if sess.monitor_pending {
                        ui.add_space(10.0);
                        widgets::notice(ui, cx, NoticeKind::Neutral, "loader-circle", |ui| {
                            label(ui, "Switching display…", theme::body(13.0), theme::INK_MUTED);
                        });
                    } else if let Some(err) = &sess.monitor_error {
                        ui.add_space(10.0);
                        widgets::notice(ui, cx, NoticeKind::Error, "circle-alert", |ui| {
                            label(ui, &format!("Couldn't switch display: {err}"), theme::body(13.0), theme::INK_MUTED);
                        });
                    }
                });
        });
}

/// The displays drawn to scale, bounding box centred in the map.
fn monitor_map(ui: &mut Ui, sess: &Session, o: &mut Picks) {
    let (area, _) = ui.allocate_exact_size(vec2(PICKER_INNER_W, MAP_H), Sense::hover());
    ui.painter().rect_filled(area, ICON_BOX_R, theme::VOID);
    if sess.monitors.is_empty() {
        return;
    }
    let minx = sess.monitors.iter().map(|m| m.x).min().unwrap_or(0) as f32;
    let miny = sess.monitors.iter().map(|m| m.y).min().unwrap_or(0) as f32;
    let maxx = sess.monitors.iter().map(|m| m.x + m.width as i32).max().unwrap_or(1) as f32;
    let maxy = sess.monitors.iter().map(|m| m.y + m.height as i32).max().unwrap_or(1) as f32;
    let bw = (maxx - minx).max(1.0);
    let bh = (maxy - miny).max(1.0);
    let s = ((PICKER_INNER_W - 24.0) / bw).min((MAP_H - 24.0) / bh);
    let ox = area.left() + (PICKER_INNER_W - bw * s) / 2.0;
    let oy = area.top() + (MAP_H - bh * s) / 2.0;

    // While a switch is pending the map is drawn dimmed and takes no clicks or hover.
    let live = !sess.monitor_pending;
    for (i, m) in sess.monitors.iter().enumerate() {
        let r = Rect::from_min_size(
            pos2(ox + (m.x as f32 - minx) * s, oy + (m.y as f32 - miny) * s),
            vec2(m.width as f32 * s, m.height as f32 * s),
        );
        let resp = ui.interact(r, ui.id().with(("mon", i)), if live { Sense::click() } else { Sense::hover() });
        let hot = live && resp.hovered();
        let cur = i == sess.monitor;
        let edge = if cur {
            theme::SIGNAL
        } else if hot {
            theme::INK_FAINT
        } else {
            theme::THREAD_STRONG
        };
        let fill = if cur {
            theme::SURFACE_RAISED.lerp_to_gamma(theme::SIGNAL, 0.12)
        } else {
            theme::SURFACE_RAISED
        };
        let p = ui.painter();
        p.rect_filled(r, CornerRadius::same(4), dim(live, fill));
        p.rect_stroke(r, CornerRadius::same(4), Stroke::new(1.5, dim(live, edge)), StrokeKind::Inside);
        let ink = if cur { theme::INK } else { theme::INK_MUTED };
        p.text(r.center(), Align2::CENTER_CENTER, (i + 1).to_string(), theme::display(20.0), dim(live, ink));
        if cur {
            p.text(
                pos2(r.center().x, r.bottom() - 5.0),
                Align2::CENTER_BOTTOM,
                "Showing",
                theme::bold(10.0),
                dim(live, theme::SIGNAL),
            );
        }
        if resp.clicked() {
            o.acts.push(Action::SetMonitor(i));
            o.strip = Some(StripView::Toolbar);
        }
    }
}

/// One row per display: check (current) or number key, name, mode in mono.
fn monitor_rows(ui: &mut Ui, cx: &Cx, sess: &Session, o: &mut Picks) {
    // While a switch is pending the rows are drawn dimmed and take no clicks or hover.
    let live = !sess.monitor_pending;
    for (i, m) in sess.monitors.iter().enumerate() {
        let (rect, _) = ui.allocate_exact_size(vec2(PICKER_INNER_W, 40.0), Sense::hover());
        let resp = ui.interact(rect, ui.id().with(("mrow", i)), if live { Sense::click() } else { Sense::hover() });
        let hot = live && resp.hovered();
        let cur = i == sess.monitor;
        let p = ui.painter();
        if cur {
            p.rect_filled(rect, theme::R_FIELD, dim(live, theme::SURFACE_MUTED));
        } else if hot {
            p.rect_filled(rect, theme::R_FIELD, theme::SURFACE_RAISED);
        }
        let cy = rect.center().y;
        let mut x = rect.left() + 10.0;
        if cur {
            cx.icons.paint(p, Rect::from_center_size(pos2(x + 8.0, cy), vec2(16.0, 16.0)), "check", dim(live, theme::SIGNAL));
        } else {
            let key = Rect::from_min_size(pos2(x, cy - 9.0), vec2(18.0, 18.0));
            p.rect_filled(key, CornerRadius::same(4), dim(live, theme::SURFACE_RAISED));
            p.text(key.center(), Align2::CENTER_CENTER, (i + 1).to_string(), theme::mono(10.5), dim(live, theme::INK_MUTED));
        }
        x += 32.0;
        let name = p.text(pos2(x, cy), Align2::LEFT_CENTER, format!("Display {}", i + 1), theme::bold(13.0), dim(live, theme::INK));
        if m.primary {
            p.text(pos2(name.right(), cy), Align2::LEFT_CENTER, " · main", theme::body(13.0), dim(live, theme::INK_MUTED));
        }
        p.text(
            pos2(rect.right() - 10.0, cy),
            Align2::RIGHT_CENTER,
            format!("{} x {} · {} Hz", m.width, m.height, m.refresh_hz),
            theme::mono(11.0),
            dim(live, theme::INK_FAINT),
        );
        if resp.clicked() {
            o.acts.push(Action::SetMonitor(i));
            o.strip = Some(StripView::Toolbar);
        }
    }
}

// ------------------------------------------------------------ stats

/// Evidence panel, top right, floating under the strip or tab with a margin
/// from the window edge so it is never clipped.
fn stats_panel(ctx: &egui::Context, sess: &Session, top: f32) {
    // Sparkline moves while open; cap the repaint at 10 Hz.
    ctx.request_repaint_after(Duration::from_millis(100));
    let s = &sess.stats;
    egui::Area::new(Id::new("session.stats"))
        .order(Order::Foreground)
        .anchor(Align2::RIGHT_TOP, vec2(-STATS_MARGIN, top + POP_GAP))
        .show(ctx, |ui| {
            Frame::NONE
                .fill(tint(theme::VOID, 224))
                .stroke(Stroke::new(1.0, theme::THREAD_STRONG))
                .corner_radius(theme::R_PANEL)
                .shadow(theme::CHROME_SHADOW)
                .inner_margin(Margin { left: 14, right: 14, top: 10, bottom: 12 })
                .show(ui, |ui| {
                    ui.set_width(240.0);
                    ui.horizontal(|ui| {
                        label(ui, "Stream", theme::bold(12.0), theme::INK);
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            label(ui, "Ctrl+Alt+Shift+S", theme::mono(10.0), theme::INK_FAINT);
                        });
                    });
                    ui.add_space(6.0);
                    sparkline(ui, &s.frame_ms);
                    ui.horizontal(|ui| {
                        label(ui, "frame time, last 4 s", theme::mono(10.0), theme::INK_FAINT);
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            label(ui, "16.7 ms line", theme::mono(10.0), theme::INK_FAINT);
                        });
                    });
                    ui.add_space(6.0);
                    egui::Grid::new("session.stats.rows")
                        .num_columns(2)
                        .spacing([12.0, 0.0])
                        .show(ui, |ui| {
                            kv(ui, "round trip", &format!("{} ms", s.rtt_ms), s.rtt_ms <= GOOD_RTT_MS);
                            kv(ui, "decode", &format!("{:.1} ms", s.decode_ms), false);
                            kv(ui, "frames", &format!("{:.1} fps", s.fps), false);
                            kv(ui, "dropped", &s.dropped.to_string(), false);
                            kv(ui, "bitrate", &format!("{:.1} Mb/s", s.bitrate_mbps), false);
                            kv(ui, "resolution", &s.resolution, false);
                            kv(ui, "transport", &s.transport, false);
                            kv(ui, "codec", &s.codec, false);
                        });
                });
        });
}

fn kv(ui: &mut Ui, key: &str, value: &str, good: bool) {
    label(ui, key, theme::mono(11.0), theme::INK_FAINT);
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        let color = if good { theme::CONSTRUCTIVE } else { theme::INK };
        label(ui, value, theme::mono(11.0), color);
    });
    ui.end_row();
}

/// Frame times as line segments; the 16.7 ms line is dashed.
fn sparkline(ui: &mut Ui, frames: &[f32]) {
    let (r, _) = ui.allocate_exact_size(vec2(240.0, 34.0), Sense::hover());
    let p = ui.painter();
    let y_of = |ms: f32| r.bottom() - (ms / FRAME_SCALE_MS).clamp(0.0, 1.0) * r.height();
    let mid = y_of(16.7);
    let dash = Stroke::new(1.0, theme::THREAD);
    let mut x = r.left();
    while x < r.right() {
        p.line_segment([pos2(x, mid), pos2((x + 2.0).min(r.right()), mid)], dash);
        x += 5.0;
    }
    p.line_segment([r.left_bottom(), r.right_bottom()], Stroke::new(1.0, theme::THREAD));
    let tail = &frames[frames.len().saturating_sub(SPARK_N)..];
    if tail.len() >= 2 {
        let step = r.width() / (tail.len() - 1) as f32;
        let pts: Vec<Pos2> = tail
            .iter()
            .enumerate()
            .map(|(i, v)| pos2(r.left() + step * i as f32, y_of(*v)))
            .collect();
        for w in pts.windows(2) {
            p.line_segment([w[0], w[1]], Stroke::new(1.5, theme::SIGNAL));
        }
    }
}

// ------------------------------------------------------------ toasts

/// Capture and released toasts, centred under the chrome. No timers in the
/// demo; the line along the bottom is the mockup's static 64% mark.
fn toast(ctx: &egui::Context, cx: &Cx, sess: &Session, which: Toast, top: f32) {
    let captured = which == Toast::Captured;
    let (icon, fg, box_bg) = if captured {
        ("mouse-pointer-click", theme::SIGNAL, tint(theme::SIGNAL, 36))
    } else {
        ("mouse-pointer-2", theme::INK_MUTED, theme::SURFACE_MUTED)
    };
    let title = if captured {
        format!("Mouse and keyboard now go to {}", sess.pc_name)
    } else {
        "Mouse and keyboard are back on this computer".to_owned()
    };
    egui::Area::new(Id::new("session.toast"))
        .order(Order::Foreground)
        .anchor(Align2::CENTER_TOP, vec2(0.0, top))
        .show(ctx, |ui| {
            let f = Frame::NONE
                .fill(theme::CHROME_BG)
                .stroke(Stroke::new(1.0, theme::THREAD_STRONG))
                .corner_radius(theme::R_PANEL)
                .shadow(theme::CHROME_SHADOW)
                .inner_margin(Margin { left: 14, right: 16, top: 12, bottom: 12 })
                .show(ui, |ui| {
                    ui.set_min_width(440.0);
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 12.0;
                        let (b, _) = ui.allocate_exact_size(vec2(34.0, 34.0), Sense::hover());
                        ui.painter().rect_filled(b, ICON_BOX_R, box_bg);
                        cx.icons.paint(ui.painter(), Rect::from_center_size(b.center(), vec2(18.0, 18.0)), icon, fg);
                        ui.vertical(|ui| {
                            label(ui, &title, theme::bold(14.0), theme::INK);
                            ui.add_space(3.0);
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 8.0;
                                if captured {
                                    label(ui, "Release with", theme::body(12.5), theme::INK_MUTED);
                                    widgets::chord(ui, &["Ctrl", "Alt", "Shift", "Z"]);
                                } else {
                                    label(ui, "Click the stream to capture again ·", theme::body(12.5), theme::INK_MUTED);
                                    widgets::chord(ui, &["Ctrl", "Alt", "Shift", "Q"]);
                                    label(ui, "leaves", theme::body(12.5), theme::INK_MUTED);
                                }
                            });
                        });
                    });
                });
            let r = f.response.rect;
            let line = Rect::from_min_size(pos2(r.left(), r.bottom() - 2.0), vec2(r.width() * 0.64, 2.0));
            let line_fg = if captured { tint(theme::SIGNAL, 179) } else { theme::INK_FAINT };
            ui.painter().rect_filled(line, CornerRadius::ZERO, line_fg);
        });
}

// ------------------------------------------------------------ banner and card

/// Admin required: hangs from the chrome, stream keeps playing.
fn banner(ctx: &egui::Context, cx: &Cx, top: f32, o: &mut Picks) {
    egui::Area::new(Id::new("session.banner"))
        .order(Order::Foreground)
        .anchor(Align2::CENTER_TOP, vec2(0.0, top - 1.0))
        .show(ctx, |ui| {
            let f = Frame::NONE
                .fill(theme::SURFACE)
                .stroke(Stroke::new(1.0, theme::THREAD_STRONG))
                .corner_radius(BANNER_R)
                .shadow(theme::CHROME_SHADOW)
                .inner_margin(Margin { left: 14, right: 14, top: 12, bottom: 12 })
                .show(ui, |ui| {
                    ui.set_width(BANNER_W - 28.0);
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 12.0;
                        widgets::icon(ui, cx, "shield-alert", 22.0, theme::ATTENTION);
                        // Text column takes what the icon and Dismiss leave; it wraps.
                        let text_w = (ui.available_width() - BANNER_DISMISS_SLOT - 12.0).max(200.0);
                        ui.vertical(|ui| {
                            ui.set_max_width(text_w);
                            ui.spacing_mut().item_spacing.y = 3.0;
                            label(ui, "This window needs Admin mode — turn it on at the PC", theme::bold(14.0), theme::INK);
                            ui.label(
                                RichText::new(
                                    "The picture keeps streaming. Clicks and keys don't reach this window until Admin mode is on; other windows still work.",
                                )
                                .font(theme::body(12.5))
                                .color(theme::INK_MUTED),
                            );
                        });
                        if widgets::button(ui, cx, BtnKind::Ghost, BtnSize::Small, None, "Dismiss").clicked() {
                            o.dismiss = true;
                        }
                    });
                });
            let r = f.response.rect;
            ui.painter().rect_filled(
                Rect::from_min_size(r.left_top(), vec2(r.width(), 2.0)),
                CornerRadius::ZERO,
                theme::ATTENTION,
            );
        });
}

/// Reconnecting: the card in the middle, the frozen frame dimmed behind it.
fn recovery(ctx: &egui::Context, cx: &Cx, sess: &Session, o: &mut Picks) {
    let (attempt, secs) = match sess.link {
        Link::Reconnecting { attempt, secs } => (attempt, secs),
        Link::Live => (0, 0),
    };
    egui::Area::new(Id::new("session.recover"))
        .order(Order::Foreground)
        .anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0))
        .show(ctx, |ui| {
            let frame = Frame::NONE
                .fill(theme::SURFACE)
                .stroke(Stroke::new(1.0, theme::THREAD_STRONG))
                .corner_radius(theme::R_PANEL)
                .shadow(theme::CHROME_SHADOW)
                .inner_margin(Margin { left: 28, right: 24, top: 22, bottom: 20 });
            let f = contract::framed(ui, "recover", frame, |ui| {
                    ui.set_width(460.0 - 52.0);
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 10.0;
                        widgets::icon(ui, cx, "refresh-cw", 22.0, theme::PAW);
                        ui.label(
                            RichText::new(format!("Reconnecting to {}", sess.pc_name))
                                .font(theme::display(24.0))
                                .color(theme::INK),
                        );
                    });
                    ui.add_space(8.0);
                    for text in [
                        "The connection dropped. Apps on the PC keep running, and your sign-in is still valid.",
                        "Input is paused until the stream is back. The picture behind this card is the last frame, not live.",
                    ] {
                        ui.label(RichText::new(text).font(theme::body(13.5)).color(theme::INK_MUTED));
                        ui.add_space(6.0);
                    }
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 14.0;
                        label(ui, &format!("attempt {attempt}"), theme::mono(11.0), theme::INK_FAINT);
                        label(ui, &format!("next try in {secs} s"), theme::mono(11.0), theme::INK_FAINT);
                    });
                    ui.add_space(16.0);
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 10.0;
                        if widgets::button(ui, cx, BtnKind::Primary, BtnSize::Small, Some("refresh-cw"), "Try now").clicked() {
                            o.acts.push(Action::Retry);
                        }
                        if widgets::button(ui, cx, BtnKind::Ghost, BtnSize::Small, Some("log-out"), "Leave session").clicked() {
                            o.acts.push(Action::Disconnect);
                        }
                    });
                });
            let r = f.response.rect;
            ui.painter().rect_filled(
                Rect::from_min_size(r.left_top(), vec2(5.0, r.height())),
                // Follows the card's rounded left corners (a square seam
                // poked out past them).
                CornerRadius { nw: theme::R_PANEL.nw, sw: theme::R_PANEL.sw, ne: 0, se: 0 },
                theme::PAW,
            );
        });
}
