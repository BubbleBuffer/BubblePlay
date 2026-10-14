//! Home A: the "This PC" pinned section, the "Your PCs" list and the
//! right-hand panel (selected PC detail, vouch request, or the This PC host
//! panel). Mockup: client-win/mockups/connect.html states home, vouch,
//! this-pc, this-pc-request, this-pc-answered.
//!
//! Selection lives in `UiState::view`. Choices the model has no action for
//! (the vouch permission switches) and the request clocks are kept in egui's
//! temp memory, keyed by request ID.

use std::time::{Duration, Instant};

use egui::text::CCursor;
use egui::text_selection::CCursorRange;
use egui::{pos2, vec2, Align, Color32, Frame, Id, Key, Layout, Margin, Painter, Rect, RichText, Sense, Stroke, StrokeKind, TextEdit, UiBuilder};

use super::theme::{self, size};
use super::widgets::{self, BtnKind, BtnSize, PillKind};
use super::{Cx, HomeSel, UiState, View};
use crate::model::{
    Action, AnsweredRequest, AppState, BuildState, HostRun, HostSetting, HostStatus, PairedDevice,
    PairingRequest, PcKind, RemotePc, StreamMode, VouchRequest,
};

/// Height of one PC row (name line, mono ID line, padding). The text block
/// inside is `ROW_H - 2 * ROW_PAD_Y` tall and both lines must fit in it.
const ROW_H: f32 = 64.0;
const ROW_PAD_Y: f32 = 10.0;
/// Width kept at the right of a row for its Connect button or pill.
const END_W: f32 = 150.0;

/// What the right-hand side and the list highlight.
enum Target {
    Pc(usize),
    ThisPc,
    Nothing,
}

pub fn show(ui: &mut egui::Ui, cx: &mut Cx, state: &AppState, us: &mut UiState) {
    let target = resolve(state, us);
    let this_pc = matches!(target, Target::ThisPc);
    let full = ui.max_rect();
    let list_w = if this_pc { 350.0 } else { (full.width() - theme::DETAIL_W).max(0.0) };
    let list = Rect::from_min_max(full.min, pos2(full.min.x + list_w, full.max.y));
    let right = Rect::from_min_max(pos2(list.max.x, full.min.y), full.max);

    list_column(ui, cx, state, us, &target, list);

    match target {
        Target::Pc(i) => {
            let pc = &state.pcs[i];
            match &pc.vouch_request {
                Some(req) => vouch_panel(ui, cx, pc, req, right),
                None => detail_panel(ui, cx, pc, right),
            }
        }
        Target::ThisPc => {
            if let Some(host) = &state.host {
                host_panel(ui, cx, host, right);
            }
        }
        Target::Nothing => {
            place(ui, right.shrink(28.0), |ui| {
                ui.label(
                    RichText::new("No PCs yet. Pair one to connect.")
                        .font(theme::body(size::LEAD))
                        .color(theme::INK_MUTED),
                );
            });
        }
    }
}

/// The selection for this frame. `HomeSel::FirstPc` and stale IDs fall back
/// to the first PC, then This PC.
fn resolve(state: &AppState, us: &UiState) -> Target {
    let want = match &us.view {
        View::Home(sel) => sel.clone(),
        View::Settings => HomeSel::FirstPc,
    };
    match want {
        HomeSel::Pc(id) => {
            if let Some(i) = state.pcs.iter().position(|p| p.id == id) {
                return Target::Pc(i);
            }
        }
        HomeSel::ThisPc => {
            if state.host.is_some() {
                return Target::ThisPc;
            }
        }
        HomeSel::FirstPc => {}
    }
    if !state.pcs.is_empty() {
        Target::Pc(0)
    } else if state.host.is_some() {
        Target::ThisPc
    } else {
        Target::Nothing
    }
}

// ------------------------------------------------------------ placement

/// A child UI laid out top-down inside `rect`. Does not move the parent's
/// cursor, so rects can be placed in any order.
fn place(ui: &mut egui::Ui, rect: Rect, add: impl FnOnce(&mut egui::Ui)) {
    place_layout(ui, rect, Layout::top_down(Align::Min), add);
}

fn place_layout(ui: &mut egui::Ui, rect: Rect, layout: Layout, add: impl FnOnce(&mut egui::Ui)) {
    let mut child = ui.new_child(UiBuilder::new().max_rect(rect).layout(layout));
    add(&mut child);
}

/// A 1 px horizontal rule in `color`, full width.
fn seam(ui: &mut egui::Ui, color: Color32) {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(w, 1.0), Sense::hover());
    ui.painter().rect_filled(rect, 0.0, color);
}

/// Left-edge vertical rule (the panel border).
fn panel_border(p: &Painter, rect: Rect) {
    p.vline(rect.min.x, rect.y_range(), Stroke::new(1.0, theme::THREAD));
}

fn meta_text(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).font(theme::body(size::SMALL)).color(theme::INK_FAINT));
}

// ------------------------------------------------------------ list column

fn list_column(ui: &mut egui::Ui, cx: &mut Cx, state: &AppState, us: &mut UiState, target: &Target, rect: Rect) {
    let this_pc = matches!(target, Target::ThisPc);
    let selected_pc = match target {
        Target::Pc(i) => Some(*i),
        _ => None,
    };
    let pad_r = if this_pc { 20.0 } else { 32.0 };
    // The footer keeps 16 px to the panel edge in the narrow This PC rail, so
    // "Copy" stays inside the 350 px column.
    let foot_r = if this_pc { 16.0 } else { 32.0 };
    let foot = Rect::from_min_max(
        pos2(rect.min.x + 36.0, rect.max.y - 18.0 - 32.0),
        pos2(rect.max.x - foot_r, rect.max.y - 18.0),
    );
    // The body starts 16 px in so the selection marker (10 px outside the
    // row) is not clipped; the frame below restores the 36 px content edge.
    let body = Rect::from_min_max(
        pos2(rect.min.x + 16.0, rect.min.y + 30.0),
        pos2(rect.max.x - pad_r, foot.min.y - 12.0),
    );

    // The bar shows only when the list overflows. Overflow is read from the
    // previous frame's content size; while hidden the list still scrolls.
    let overflow_key = Id::new("home_list_overflow");
    let overflow = ui.ctx().data(|d| d.get_temp::<bool>(overflow_key)).unwrap_or(false);
    let bar = if overflow {
        egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded
    } else {
        egui::scroll_area::ScrollBarVisibility::AlwaysHidden
    };
    place(ui, body, |ui| {
        let out = egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .scroll_bar_visibility(bar)
            .show(ui, |ui| {
                Frame::NONE
                    .inner_margin(Margin { left: 20, right: 0, top: 0, bottom: 0 })
                    .show(ui, |ui| {
                        if let Some(host) = &state.host {
                            pinned_section(ui, cx, host, this_pc, us);
                        }
                        list_head(ui, cx, this_pc);
                        for (i, pc) in state.pcs.iter().enumerate() {
                            pc_row(ui, cx, pc, selected_pc == Some(i), this_pc, us);
                            ui.add_space(2.0);
                        }
                        if state.pcs.is_empty() {
                            meta_text(ui, "No PCs paired yet.");
                        }
                    });
            });
        let over = out.content_size.y > out.inner_rect.height() + 0.5;
        ui.ctx().data_mut(|d| d.insert_temp(overflow_key, over));
    });

    place(ui, foot, |ui| list_foot(ui, cx, state, us, this_pc));
}

fn pinned_section(ui: &mut egui::Ui, cx: &mut Cx, host: &HostStatus, this_pc: bool, us: &mut UiState) {
    ui.horizontal(|ui| {
        widgets::icon(ui, cx, "radio-tower", theme::ICON_SM, theme::INK_FAINT);
        widgets::section_label(ui, "This PC");
    });
    ui.add_space(6.0);
    host_row(ui, cx, host, this_pc, us);
    ui.add_space(16.0);
    seam(ui, theme::THREAD_STRONG);
    ui.add_space(18.0);
}

fn list_head(ui: &mut egui::Ui, cx: &mut Cx, this_pc: bool) {
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.label(RichText::new("Your PCs").font(theme::display(size::LIST_TITLE)).color(theme::INK));
            ui.label(
                RichText::new("Other PCs paired with this device's key.")
                    .font(theme::body(size::LABEL))
                    .color(theme::INK_FAINT),
            );
        });
        if !this_pc {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if widgets::button(ui, cx, BtnKind::Secondary, BtnSize::Normal, Some("plus"), "Pair a new PC")
                    .clicked()
                {
                    cx.actions.push(Action::StartPairing);
                }
            });
        }
    });
    ui.add_space(20.0);
}

/// Icon box, text column and the right-hand end column of a row.
fn row_parts(rect: Rect) -> (Rect, Rect, Rect) {
    let icon = Rect::from_min_size(pos2(rect.min.x + 12.0, rect.center().y - 20.0), vec2(40.0, 40.0));
    let end = Rect::from_min_max(
        pos2(rect.max.x - 12.0 - END_W, rect.min.y),
        pos2(rect.max.x - 12.0, rect.max.y),
    );
    let text = Rect::from_min_max(
        pos2(icon.max.x + 14.0, rect.min.y + ROW_PAD_Y),
        pos2(end.min.x - 8.0, rect.max.y - ROW_PAD_Y),
    );
    (icon, text, end)
}

/// The 3 px marker that sits left of a selected row.
fn marker(p: &Painter, rect: Rect, color: Color32) {
    let m = Rect::from_min_max(pos2(rect.min.x - 10.0, rect.min.y + 12.0), pos2(rect.min.x - 7.0, rect.max.y - 12.0));
    p.rect_filled(m, 2.0, color);
}

fn row_fill(rect: Rect, p: &Painter, selected: bool, hovered: bool) {
    let fill = if selected {
        theme::SURFACE_MUTED
    } else if hovered {
        theme::SURFACE_RAISED
    } else {
        Color32::TRANSPARENT
    };
    p.rect_filled(rect, theme::R_FIELD, fill);
}

fn icon_box(ui: &mut egui::Ui, cx: &Cx, p: &Painter, icon: Rect, name: &str, bg: Color32, fg: Color32) {
    p.rect_filled(icon, theme::R_FIELD, bg);
    place(ui, icon.shrink(9.0), |ui| {
        widgets::icon(ui, cx, name, theme::ICON_LG, fg);
    });
}

fn run_state(run: &HostRun) -> (PillKind, String) {
    match run {
        HostRun::Running => (PillKind::Online, "Running".into()),
        HostRun::Starting => (PillKind::Live, "Starting".into()),
        HostRun::Restarting { in_secs } => (PillKind::Live, format!("Restarting in {in_secs} s")),
        HostRun::Stopped => (PillKind::Off, "Stopped".into()),
        HostRun::NotConfigured => (PillKind::Off, "Not set up".into()),
    }
}

/// Long form for the This PC panel.
fn viewer_line(host: &HostStatus) -> String {
    match &host.viewer {
        Some(v) => format!("{} connected since {}", v.name, v.since),
        None => "No viewer connected".into(),
    }
}

/// Short form for the pinned row in Your PCs.
fn viewer_short(host: &HostStatus) -> String {
    match &host.viewer {
        Some(v) => format!("{} connected", v.name),
        None => "No viewer connected".into(),
    }
}

fn host_row(ui: &mut egui::Ui, cx: &mut Cx, host: &HostStatus, this_pc: bool, us: &mut UiState) {
    let selected = this_pc;
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    let p = ui.painter().clone();
    row_fill(rect, &p, selected, resp.hovered());
    if selected {
        marker(&p, rect, theme::MOSS);
    }
    let (icon, text, end) = row_parts(rect);
    let (bg, fg) = if selected {
        (theme::MOSS, theme::INK)
    } else {
        (theme::MOSS.gamma_multiply(0.35), theme::CONSTRUCTIVE)
    };
    icon_box(ui, cx, &p, icon, "server", bg, fg);

    let (run_kind, run_word) = run_state(&host.run);
    let run_word = if matches!(host.run, HostRun::Running) { "Hosting".to_string() } else { run_word };
    place(ui, text, |ui| {
        ui.spacing_mut().item_spacing.y = 3.0;
        ui.horizontal(|ui| {
            ui.label(RichText::new(&host.pc_name).font(theme::bold(size::ROW_NAME)).color(theme::INK));
            widgets::pill(ui, run_kind, &run_word);
        });
        ui.horizontal(|ui| {
            if this_pc {
                if host.request.is_some() {
                    widgets::pill(ui, PillKind::Warn, "1 pairing request");
                } else if let Some(ans) = &host.answered {
                    meta_text(ui, &format!("Request answered on {}", ans.answered_on));
                } else if host.viewer.is_some() {
                    meta_text(ui, "1 viewer connected");
                } else {
                    meta_text(ui, "No viewer connected");
                }
            } else {
                meta_text(ui, &viewer_short(host));
                let admin = if host.settings.admin_mode { "Admin mode on" } else { "Admin mode off" };
                meta_text(ui, admin);
            }
        });
    });

    let mut manage = false;
    if !this_pc {
        place_layout(ui, end, Layout::right_to_left(Align::Center), |ui| {
            manage = widgets::button(ui, cx, BtnKind::Ghost, BtnSize::Small, Some("settings"), "Manage").clicked();
        });
    }
    if manage || resp.clicked() {
        us.view = View::Home(HomeSel::ThisPc);
    }
}

fn pc_meta(pc: &RemotePc) -> String {
    if pc.online {
        format!("Last mode {}", mode_word(pc.last_mode.unwrap_or_default()))
    } else {
        format!("Last seen {}", pc.last_seen.as_deref().unwrap_or("never"))
    }
}

fn mode_word(mode: StreamMode) -> &'static str {
    match mode {
        StreamMode::Work => "Work",
        StreamMode::Game => "Game",
    }
}

fn pc_row(ui: &mut egui::Ui, cx: &mut Cx, pc: &RemotePc, selected: bool, this_pc: bool, us: &mut UiState) {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    let p = ui.painter().clone();
    row_fill(rect, &p, selected, resp.hovered());
    if selected {
        marker(&p, rect, theme::SIGNAL);
    }
    let (icon, text, end) = row_parts(rect);
    let (bg, fg) = if selected {
        (theme::INK, theme::SIGNAL_INK)
    } else {
        (theme::SURFACE_MUTED, theme::INK_MUTED)
    };
    let kind_icon = match pc.kind {
        PcKind::Desktop => "monitor",
        PcKind::Laptop => "laptop",
    };
    icon_box(ui, cx, &p, icon, kind_icon, bg, fg);

    let (pill_kind, pill_word) = if pc.online {
        (PillKind::Online, "Online")
    } else {
        (PillKind::Off, "Offline")
    };
    let edit_key = rename_key(&pc.id);
    let editing = ui.ctx().data(|d| d.get_temp::<RenameEdit>(edit_key));
    place(ui, text, |ui| {
        ui.spacing_mut().item_spacing.y = 3.0;
        ui.horizontal(|ui| {
            match editing {
                Some(edit) => rename_field(ui, cx, pc, edit_key, edit),
                None => {
                    ui.label(RichText::new(&pc.name).font(theme::bold(size::ROW_NAME)).color(theme::INK));
                }
            }
            widgets::pill(ui, pill_kind, pill_word);
        });
        ui.horizontal(|ui| {
            widgets::id_text(ui, &pc.id, false);
            if !this_pc {
                meta_text(ui, &pc_meta(pc));
            }
        });
    });

    let mut connect = false;
    if !this_pc {
        place_layout(ui, end, Layout::right_to_left(Align::Center), |ui| {
            if pc.vouch_request.is_some() {
                widgets::pill(ui, PillKind::Warn, "1 pairing request");
            } else if pc.online {
                let kind = if selected { BtnKind::Primary } else { BtnKind::Secondary };
                connect = widgets::button(ui, cx, kind, BtnSize::Normal, Some("monitor-play"), "Connect").clicked();
            } else {
                widgets::button_disabled(ui, cx, BtnKind::Secondary, BtnSize::Normal, Some("monitor-play"), "Connect");
            }
        });
    }

    if connect {
        cx.actions.push(Action::Connect { pc_id: pc.id.clone() });
    } else if resp.clicked() {
        us.view = View::Home(HomeSel::Pc(pc.id.clone()));
    }
}

/// An open inline rename on a device row, kept in egui temp memory by PC ID.
#[derive(Clone)]
struct RenameEdit {
    text: String,
    /// Set by the Rename button: the next frame focuses the field and selects its text.
    armed: bool,
}

fn rename_key(pc_id: &str) -> Id {
    Id::new(("home_rename", pc_id))
}

/// Opens the inline rename for one PC; the row shows the field from the next frame.
fn start_rename(ctx: &egui::Context, pc: &RemotePc) {
    let edit = RenameEdit { text: pc.name.clone(), armed: true };
    ctx.data_mut(|d| d.insert_temp(rename_key(&pc.id), edit));
    ctx.request_repaint();
}

/// The row's name as a single-line input. Enter commits (trimmed; ignored when
/// empty or unchanged), Esc or losing focus cancels.
fn rename_field(ui: &mut egui::Ui, cx: &mut Cx, pc: &RemotePc, key: Id, mut edit: RenameEdit) {
    let text_id = key.with("text");
    if edit.armed {
        edit.armed = false;
        ui.memory_mut(|m| m.request_focus(text_id));
        let mut st = TextEdit::load_state(ui.ctx(), text_id).unwrap_or_default();
        let len = edit.text.chars().count();
        st.cursor.set_char_range(Some(CCursorRange::two(CCursor::new(0usize), CCursor::new(len))));
        st.store(ui.ctx(), text_id);
    }

    let frame = Frame::NONE
        .fill(theme::SURFACE_RAISED)
        .stroke(Stroke::new(1.0, theme::THREAD_STRONG))
        .corner_radius(theme::R_FIELD)
        .inner_margin(Margin::symmetric(6, 0));
    let out = frame.show(ui, |ui| {
        TextEdit::singleline(&mut edit.text)
            .id(text_id)
            .font(theme::bold(size::ROW_NAME))
            .text_color(theme::INK)
            .frame(Frame::NONE)
            .desired_width(180.0)
            .show(ui)
            .response
    });
    let input = out.inner;
    let rect = out.response.rect;
    if input.has_focus() {
        ui.painter().rect_stroke(rect.expand(1.0), theme::R_FIELD, Stroke::new(2.0, theme::FOCUS), StrokeKind::Outside);
    }

    let escape = ui.input(|i| i.key_pressed(Key::Escape));
    let enter = ui.input(|i| i.key_pressed(Key::Enter));
    let finished = if escape {
        true
    } else if input.lost_focus() {
        // Enter also drops focus; a click elsewhere only cancels.
        if enter {
            let name = edit.text.trim();
            if !name.is_empty() && name != pc.name {
                cx.actions.push(Action::Rename { pc_id: pc.id.clone(), name: name.to_string() });
            }
        }
        true
    } else {
        false
    };
    if finished {
        ui.ctx().data_mut(|d| d.remove::<RenameEdit>(key));
    } else {
        ui.ctx().data_mut(|d| d.insert_temp(key, edit));
    }
}

/// The footer row. The left group (label, ID, Copy) is laid out first, then
/// Settings on the right; the key note takes what is left and truncates, so
/// nothing paints over another item. The narrow This PC rail drops the icon
/// and the Settings/key note (mockup `.keynote` hidden) so Copy fits.
fn list_foot(ui: &mut egui::Ui, cx: &mut Cx, state: &AppState, us: &mut UiState, this_pc: bool) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = if this_pc { 6.0 } else { 8.0 };
        if !this_pc {
            widgets::icon(ui, cx, "fingerprint", theme::ICON_SM, theme::INK_FAINT);
        }
        meta_text(ui, "This device");
        widgets::id_text(ui, &state.this_device.id, false);
        if widgets::button(ui, cx, BtnKind::Ghost, BtnSize::Small, Some("copy"), "Copy").clicked() {
            ui.ctx().copy_text(state.this_device.id.clone());
        }
        if !this_pc {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if widgets::button(ui, cx, BtnKind::Ghost, BtnSize::Small, Some("settings"), "Settings").clicked() {
                    us.view = View::Settings;
                }
                ui.add_space(4.0);
                ui.add(
                    egui::Label::new(
                        RichText::new("Key stored with Windows data protection")
                            .font(theme::body(size::SMALL))
                            .color(theme::INK_FAINT),
                    )
                    .truncate(),
                );
            });
        }
    });
}

// ------------------------------------------------------------ helpers

/// Seconds left on a request. The clock starts the first time this screen
/// shows the request, counting down from the value the backend sent.
fn countdown(ui: &egui::Ui, key: Id, total: u32) -> u32 {
    let start = ui.ctx().data_mut(|d| *d.get_temp_mut_or_insert_with(key, Instant::now));
    ui.ctx().request_repaint_after(Duration::from_millis(500));
    total.saturating_sub(start.elapsed().as_secs() as u32)
}

fn fmt_mmss(secs: u32) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

fn fact_key(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).font(theme::body(size::LABEL)).color(theme::INK_FAINT));
}

fn fact_text(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).font(theme::body(size::LABEL)).color(theme::INK));
}

fn last_session(pc: &RemotePc) -> String {
    match &pc.last_seen {
        None => "Never connected".into(),
        Some(s) => format!("{s}, {} mode", mode_word(pc.last_mode.unwrap_or_default())),
    }
}

fn build_word(state: BuildState) -> &'static str {
    match state {
        BuildState::NoRecord => "no record yet",
        BuildState::Pending => "pending",
        BuildState::Probation => "on probation",
        BuildState::Good => "good",
        BuildState::RolledBack => "rolled back",
        BuildState::Failed => "failed",
    }
}

// ------------------------------------------------------------ detail (PC)

fn detail_panel(ui: &mut egui::Ui, cx: &mut Cx, pc: &RemotePc, rect: Rect) {
    let p = ui.painter().clone();
    p.rect_filled(rect, 0.0, theme::SURFACE);
    panel_border(&p, rect);

    let inner = Rect::from_min_max(rect.min + vec2(28.0, 28.0), rect.max - vec2(28.0, 22.0));
    let bottom_h = 200.0;
    let top = Rect::from_min_max(inner.min, pos2(inner.max.x, inner.max.y - bottom_h));
    let bottom = Rect::from_min_max(pos2(inner.min.x, inner.max.y - bottom_h), inner.max);

    place(ui, top, |ui| {
        widgets::section_label(ui, "Selected");
        ui.add_space(8.0);
        ui.label(RichText::new(&pc.name).font(theme::display(size::PANEL_NAME)).color(theme::INK));
        widgets::id_text(ui, &pc.id, true);
        ui.add_space(14.0);
        egui::Grid::new("home_detail_facts").num_columns(2).spacing([16.0, 9.0]).show(ui, |ui| {
            fact_key(ui, "Relay");
            widgets::mono_text(ui, &pc.relay, 12.0, theme::INK);
            ui.end_row();
            fact_key(ui, "Allowed");
            fact_text(ui, &pc.allowed);
            ui.end_row();
            fact_key(ui, "Last session");
            fact_text(ui, &last_session(pc));
            ui.end_row();
        });
    });

    place(ui, bottom, |ui| {
        ui.with_layout(Layout::top_down_justified(Align::Center), |ui| {
            let label = format!("Connect to {}", pc.name);
            if pc.online {
                if widgets::button(ui, cx, BtnKind::Primary, BtnSize::Large, Some("monitor-play"), &label).clicked() {
                    cx.actions.push(Action::Connect { pc_id: pc.id.clone() });
                }
            } else {
                widgets::button_disabled(ui, cx, BtnKind::Primary, BtnSize::Large, Some("monitor-play"), &label);
            }
            ui.add_space(10.0);
            for (what, keys) in [
                ("Release mouse and keyboard", ["Ctrl", "Alt", "Shift", "Z"]),
                ("Leave the session", ["Ctrl", "Alt", "Shift", "Q"]),
            ] {
                ui.horizontal(|ui| {
                    meta_text(ui, what);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        widgets::chord(ui, &keys);
                    });
                });
                ui.add_space(6.0);
            }
            ui.add_space(8.0);
            widgets::divider(ui);
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if widgets::button(ui, cx, BtnKind::Ghost, BtnSize::Small, Some("pencil"), "Rename").clicked() {
                    start_rename(ui.ctx(), pc);
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let forget = format!("Forget {}", pc.name);
                    if widgets::button(ui, cx, BtnKind::Danger, BtnSize::Small, Some("trash-2"), &forget).clicked() {
                        cx.actions.push(Action::Forget { pc_id: pc.id.clone() });
                    }
                });
            });
        });
    });
}

// ------------------------------------------------------------ vouch request

fn vouch_panel(ui: &mut egui::Ui, cx: &mut Cx, pc: &RemotePc, req: &VouchRequest, rect: Rect) {
    let p = ui.painter().clone();
    p.rect_filled(rect, 0.0, theme::SURFACE);
    panel_border(&p, rect);
    p.rect_filled(Rect::from_min_max(rect.min, pos2(rect.min.x + 4.0, rect.max.y)), 0.0, theme::ATTENTION);

    let inner = Rect::from_min_max(rect.min + vec2(32.0, 26.0), rect.max - vec2(28.0, 22.0));

    // Permission switches: local choices until "Numbers match, vouch".
    let perm_key = Id::new(("home_vouch_perms", req.request_id.as_str()));
    let mut on: Vec<bool> = ui.ctx().data_mut(|d| {
        d.get_temp_mut_or_insert_with(perm_key, || req.permissions.iter().map(|p| p.granted).collect::<Vec<bool>>())
            .clone()
    });
    if on.len() != req.permissions.len() {
        on = req.permissions.iter().map(|p| p.granted).collect();
    }
    let mut changed = false;
    let left = countdown(ui, Id::new(("home_vouch_seen", req.request_id.as_str())), req.expires_in_secs);

    place(ui, inner, |ui| {
        widgets::pill(ui, PillKind::Warn, &format!("Pairing request · {}", pc.name));
        ui.add_space(6.0);
        ui.label(
            RichText::new(format!("Vouch for {}?", req.device_name))
                .font(theme::display(24.0))
                .color(theme::INK),
        );
        ui.label(
            RichText::new(format!(
                "A new device wants to pair with {}. Only vouch if you know it is yours or someone you trust.",
                pc.name
            ))
            .font(theme::body(size::LABEL))
            .color(theme::INK_MUTED),
        );
        ui.add_space(14.0);
        egui::Grid::new("home_vouch_facts").num_columns(2).spacing([16.0, 9.0]).show(ui, |ui| {
            fact_key(ui, "Device");
            fact_text(ui, &req.device_name);
            ui.end_row();
            fact_key(ui, "ID");
            widgets::id_text(ui, &req.device_id, false);
            ui.end_row();
        });
        ui.add_space(14.0);
        widgets::number(ui, req.number, size::NUMBER);
        ui.label(
            RichText::new(format!("The number on {} must match exactly.", req.device_name))
                .font(theme::body(size::LABEL))
                .color(theme::INK_MUTED),
        );
        ui.add_space(14.0);

        for (i, perm) in req.permissions.iter().enumerate() {
            ui.horizontal(|ui| {
                fact_text(ui, &perm.label);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if widgets::switch(ui, on[i]).clicked() {
                        on[i] = !on[i];
                        changed = true;
                    }
                });
            });
            ui.add_space(8.0);
        }
        ui.horizontal(|ui| {
            meta_text(ui, "Before sign-in (only at the PC)");
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_enabled_ui(false, |ui| {
                    widgets::switch(ui, false);
                });
            });
        });

        ui.add_space(18.0);
        ui.with_layout(Layout::top_down_justified(Align::Center), |ui| {
            if widgets::button(ui, cx, BtnKind::Primary, BtnSize::Large, Some("user-check"), "Numbers match, vouch")
                .clicked()
            {
                let permissions: Vec<String> = req
                    .permissions
                    .iter()
                    .zip(on.iter())
                    .filter(|(_, granted)| **granted)
                    .map(|(perm, _)| perm.key.clone())
                    .collect();
                cx.actions.push(Action::Vouch {
                    request_id: req.request_id.clone(),
                    accept: true,
                    permissions,
                });
            }
            if widgets::button(ui, cx, BtnKind::Secondary, BtnSize::Normal, None, "Deny").clicked() {
                cx.actions.push(Action::Vouch {
                    request_id: req.request_id.clone(),
                    accept: false,
                    permissions: Vec::new(),
                });
            }
            ui.horizontal_wrapped(|ui| {
                meta_text(ui, "Request expires in");
                widgets::mono_text(ui, &fmt_mmss(left), size::SMALL, theme::INK_FAINT);
                meta_text(ui, &format!(". You can vouch because {} allows this device to.", pc.name));
            });
        });
    });

    if changed {
        ui.ctx().data_mut(|d| d.insert_temp(perm_key, on));
    }
}

// ------------------------------------------------------------ This PC

fn host_panel(ui: &mut egui::Ui, cx: &mut Cx, host: &HostStatus, rect: Rect) {
    let p = ui.painter().clone();
    p.rect_filled(rect, 0.0, theme::SURFACE);
    panel_border(&p, rect);
    p.rect_filled(Rect::from_min_max(rect.min, pos2(rect.min.x + 4.0, rect.max.y)), 0.0, theme::MOSS);

    let inner = Rect::from_min_max(rect.min + vec2(30.0, 24.0), rect.max - vec2(26.0, 18.0));
    let top = Rect::from_min_max(inner.min, pos2(inner.max.x, inner.min.y + 84.0));
    let maint = Rect::from_min_max(pos2(inner.min.x, inner.max.y - 34.0), inner.max);
    let body = Rect::from_min_max(pos2(inner.min.x, top.max.y + 16.0), pos2(inner.max.x, maint.min.y - 8.0));
    p.hline(inner.x_range(), top.max.y + 6.0, Stroke::new(1.0, theme::THREAD));
    p.hline(inner.x_range(), maint.min.y, Stroke::new(1.0, theme::THREAD));

    place(ui, top, |ui| {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new("This PC").font(theme::display(32.0)).color(theme::INK));
                ui.horizontal(|ui| {
                    let (kind, word) = run_state(&host.run);
                    widgets::pill(ui, kind, &word);
                    meta_text(ui, &viewer_line(host));
                    meta_text(ui, "PC ID");
                    widgets::id_text(ui, host.pc_id.as_deref().unwrap_or("not set yet"), false);
                });
            });
            ui.with_layout(Layout::right_to_left(Align::Min), |ui| match host.run {
                HostRun::NotConfigured => {}
                HostRun::Stopped => {
                    if widgets::button(ui, cx, BtnKind::Secondary, BtnSize::Normal, Some("play"), "Start hosting")
                        .clicked()
                    {
                        cx.actions.push(Action::StartHosting);
                    }
                }
                _ => {
                    // Right-to-left: the first button added sits at the far right.
                    if widgets::button(ui, cx, BtnKind::Secondary, BtnSize::Normal, Some("circle-stop"), "Stop hosting")
                        .clicked()
                    {
                        cx.actions.push(Action::StopHosting);
                    }
                    if widgets::button(ui, cx, BtnKind::Secondary, BtnSize::Normal, Some("rotate-ccw"), "Restart")
                        .clicked()
                    {
                        cx.actions.push(Action::RestartHost);
                    }
                }
            });
        });
    });

    place(ui, body, |ui| {
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 26.0;
            ui.columns(2, |cols| {
                if let [left, right] = cols {
                    settings_column(left, cx, host);
                    devices_column(right, cx, host);
                }
            });
        });
    });

    place(ui, maint, |ui| host_footer(ui, cx, host));
}

fn settings_column(ui: &mut egui::Ui, cx: &mut Cx, host: &HostStatus) {
    widgets::section_label(ui, "Hosting settings");
    let s = host.settings;
    setting_line(
        ui,
        cx,
        "Stream PC sound",
        "Changing it restarts the host and ends the current session.",
        s.stream_sound,
        HostSetting::StreamSound,
    );
    widgets::divider(ui);
    setting_line(
        ui,
        cx,
        "Admin mode",
        "Lets input reach elevated windows. Turning it on asks for one admin prompt.",
        s.admin_mode,
        HostSetting::AdminMode,
    );
    widgets::divider(ui);
    setting_line(
        ui,
        cx,
        "Start at login",
        "Starts BubblePlay hosting when you sign in to Windows.",
        s.start_at_login,
        HostSetting::StartAtLogin,
    );
    widgets::divider(ui);
    setting_line(
        ui,
        cx,
        "CPU mode",
        "Encodes on the CPU instead of the graphics card. Takes effect after the host restarts.",
        s.cpu_mode,
        HostSetting::CpuMode,
    );
}

fn setting_line(ui: &mut egui::Ui, cx: &mut Cx, label: &str, help: &str, on: bool, make: fn(bool) -> HostSetting) {
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).font(theme::bold(size::LABEL)).color(theme::INK));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if widgets::switch(ui, on).clicked() {
                cx.actions.push(Action::SetHostSetting(make(!on)));
            }
        });
    });
    ui.label(RichText::new(help).font(theme::body(size::HINT)).color(theme::INK_FAINT));
    ui.add_space(6.0);
}

fn devices_column(ui: &mut egui::Ui, cx: &mut Cx, host: &HostStatus) {
    if let Some(req) = &host.request {
        request_card(ui, cx, req);
    }
    if let Some(ans) = &host.answered {
        answered_card(ui, cx, ans);
    }
    ui.horizontal(|ui| {
        widgets::section_label(ui, "Paired devices");
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if widgets::button(ui, cx, BtnKind::Ghost, BtnSize::Small, Some("qr-code"), "Pair a device").clicked() {
                cx.actions.push(Action::ShowPairDevice);
            }
        });
    });
    ui.add_space(4.0);
    for d in &host.paired {
        device_row(ui, cx, d);
    }
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        meta_text(ui, "Switch: may vouch for new devices");
    });
}

fn request_card(ui: &mut egui::Ui, cx: &mut Cx, req: &PairingRequest) {
    let left = countdown(ui, Id::new(("home_pair_seen", req.request_id.as_str())), req.expires_in_secs);
    let frame = Frame::NONE
        .fill(theme::SURFACE_RAISED)
        .stroke(Stroke::new(1.0, theme::ATTENTION.gamma_multiply(0.4)))
        .corner_radius(theme::R_FIELD)
        .inner_margin(Margin { left: 16, right: 14, top: 12, bottom: 12 });
    let resp = frame.show(ui, |ui| {
        ui.label(
            RichText::new(format!("Pair '{}'?", req.device_name))
                .font(theme::bold(13.5))
                .color(theme::INK),
        );
        widgets::id_text(ui, &req.device_id, false);
        widgets::number(ui, req.number, 26.0);
        meta_text(ui, &format!("Allow only if {} shows this number.", req.device_name));
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if widgets::button(ui, cx, BtnKind::Primary, BtnSize::Normal, None, "Allow").clicked() {
                cx.actions.push(Action::AnswerPairing { request_id: req.request_id.clone(), allow: true });
            }
            if widgets::button(ui, cx, BtnKind::Secondary, BtnSize::Normal, None, "Deny").clicked() {
                cx.actions.push(Action::AnswerPairing { request_id: req.request_id.clone(), allow: false });
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                widgets::mono_text(ui, &format!("{} left", fmt_mmss(left)), size::SMALL, theme::INK_FAINT);
            });
        });
    });
    let r = resp.response.rect;
    ui.painter().rect_filled(Rect::from_min_max(r.min, pos2(r.min.x + 3.0, r.max.y)), 0.0, theme::ATTENTION);
    ui.add_space(12.0);
}

fn answered_card(ui: &mut egui::Ui, cx: &mut Cx, ans: &AnsweredRequest) {
    let frame = Frame::NONE
        .stroke(Stroke::new(1.0, theme::THREAD_STRONG))
        .corner_radius(theme::R_FIELD)
        .inner_margin(Margin { left: 16, right: 14, top: 12, bottom: 12 });
    let resp = frame.show(ui, |ui| {
        ui.label(
            RichText::new(format!("Pair '{}'?", ans.request.device_name))
                .font(theme::bold(13.5))
                .color(theme::INK_FAINT),
        );
        widgets::mono_text(ui, &ans.request.device_id, size::ID, theme::INK_FAINT);
        ui.horizontal(|ui| {
            widgets::icon(ui, cx, "circle-check", theme::ICON_SM, theme::CONSTRUCTIVE);
            let verdict = if ans.allowed { "allowed" } else { "denied" };
            ui.label(
                RichText::new(format!("Answered on {}: {verdict}", ans.answered_on))
                    .font(theme::bold(size::LABEL))
                    .color(theme::CONSTRUCTIVE),
            );
        });
        meta_text(ui, "Closes by itself in a few seconds.");
    });
    let r = resp.response.rect;
    ui.painter().rect_filled(Rect::from_min_max(r.min, pos2(r.min.x + 3.0, r.max.y)), 0.0, theme::THREAD_STRONG);
    ui.add_space(12.0);
}

/// One paired device: the name line carries the name, its pill and tag on the
/// left and the may-vouch switch and Revoke on the right; the ID and the
/// allowed/last-seen detail sit on the line below.
fn device_row(ui: &mut egui::Ui, cx: &mut Cx, d: &PairedDevice) {
    ui.vertical(|ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new(&d.name).font(theme::bold(size::LABEL)).color(theme::INK));
            if d.connected {
                widgets::pill(ui, PillKind::Live, "Connected");
            }
            if let Some(by) = &d.vouched_by {
                widgets::tag(ui, &format!("vouched by {by}"), false);
            }
            // Right-to-left: Revoke at the far right, the switch left of it.
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if widgets::button(ui, cx, BtnKind::Danger, BtnSize::Small, None, "Revoke").clicked() {
                    cx.actions.push(Action::Revoke { device_id: d.id.clone() });
                }
                if widgets::switch(ui, d.may_vouch).clicked() {
                    cx.actions.push(Action::SetMayVouch { device_id: d.id.clone(), may_vouch: !d.may_vouch });
                }
            });
        });
        ui.horizontal_wrapped(|ui| {
            widgets::id_text(ui, &d.id, false);
            ui.label(
                RichText::new(format!("{} · {}", d.allowed, d.last_seen))
                    .font(theme::body(size::HINT))
                    .color(theme::INK_FAINT),
            );
        });
    });
    ui.add_space(6.0);
}

fn host_footer(ui: &mut egui::Ui, cx: &mut Cx, host: &HostStatus) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("Host build").font(theme::body(size::HINT)).color(theme::INK_FAINT));
        widgets::mono_text(ui, &host.build.hash, 11.0, theme::INK_FAINT);
        ui.label(
            RichText::new(format!(": {}", build_word(host.build.state)))
                .font(theme::body(size::HINT))
                .color(theme::INK_FAINT),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if widgets::button(ui, cx, BtnKind::Ghost, BtnSize::Small, Some("folder-open"), "Open logs folder").clicked() {
                cx.actions.push(Action::OpenLogsFolder);
            }
            if host.can_roll_back {
                if widgets::button(ui, cx, BtnKind::Ghost, BtnSize::Small, Some("undo-2"), "Roll back host").clicked() {
                    cx.actions.push(Action::RollBackHost);
                }
            } else {
                widgets::button_disabled(ui, cx, BtnKind::Ghost, BtnSize::Small, Some("undo-2"), "Roll back host");
            }
        });
    });
}
