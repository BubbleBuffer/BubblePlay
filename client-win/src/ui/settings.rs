//! Settings: this device's key (ID, key file, Export, Import, Replace key).
//! Mockup: client-win/mockups/connect.html state settings.
//!
//! Uses the flow layout (rail + pane) from flow.rs.

use egui::{pos2, vec2, Align, Layout, Rect, RichText, Ui, UiBuilder};

use super::contract;

use super::flow;
use super::theme;
use super::widgets::{self, BtnKind, BtnSize};
use super::{Cx, HomeSel, UiState, View};
use crate::model::{Action, AppState};

pub fn show(ui: &mut Ui, cx: &mut Cx, state: &AppState, us: &mut UiState) {
    let mut go_home = false;
    let back = flow::layout(
        ui,
        cx,
        &state.this_device.id,
        |ui, _cx| {
            flow::rail_title(ui, "Settings");
            flow::rail_text(ui, "This device's key");
        },
        |ui, cx| {
            flow::title_row(ui, "This device's key", None);
            flow::lead(
                ui,
                "Your PCs know this device by this key. It stays on this Windows account, encrypted with Windows data protection.",
            );
            // Never wider than the pane: set_max_width widens as well.
            let w = ui.available_width().min(620.0);
            ui.set_max_width(w);
            let id = state.this_device.id.as_str();
            line(
                ui,
                cx,
                "Device ID",
                false,
                "PCs show this ID when they pair or list this device.",
                false,
                |ui, _| {
                    widgets::id_text(ui, id, true);
                },
            );
            line(
                ui,
                cx,
                "Key file",
                false,
                "Encrypted at rest (DPAPI). Copying the file to another PC or account does not work; use Export.",
                false,
                |ui, _| {
                    contract::text(ui, RichText::new(&state.this_device.key_path).font(theme::mono(12.5)).color(theme::INK_MUTED), true);
                },
            );
            line(
                ui,
                cx,
                "Export key",
                false,
                "Saves a .bpkey file to move this device to a new computer. Anyone with the file can connect as this device, so keep it like a password.",
                false,
                |ui, cx| {
                    if widgets::button(ui, cx, BtnKind::Secondary, BtnSize::Normal, Some("download"), "Export…").clicked() {
                        cx.actions.push(Action::ExportKey);
                    }
                },
            );
            line(
                ui,
                cx,
                "Import key",
                false,
                "Replaces this device's key with one from a .bpkey file. Your PCs then lists the PCs that know the imported key.",
                false,
                |ui, cx| {
                    if widgets::button(ui, cx, BtnKind::Secondary, BtnSize::Normal, Some("upload"), "Import…").clicked() {
                        cx.actions.push(Action::ImportKey);
                    }
                },
            );
            line(
                ui,
                cx,
                "Replace key",
                true,
                "Makes a new key and ID. Each PC moves this device to the new ID the next time you connect. Use it if the key file may have been copied. If you think it leaked, also remove this device at each PC.",
                true,
                |ui, cx| {
                    if widgets::button(ui, cx, BtnKind::Secondary, BtnSize::Normal, Some("key-round"), "Replace key…")
                        .clicked()
                    {
                        cx.actions.push(Action::ReplaceKey);
                    }
                },
            );
            flow::actions(ui, |ui| {
                if widgets::button(
                    ui,
                    cx,
                    BtnKind::Ghost,
                    BtnSize::Normal,
                    None,
                    "Back to your PCs",
                )
                .clicked()
                {
                    go_home = true;
                }
            });
        },
    );
    if back || go_home {
        us.view = View::Home(HomeSel::FirstPc);
    }
}

/// One row of the settings list, as the mockup's `.set .line` grid
/// (`1fr auto`): bold label (with an optional Draft tag) over a faint
/// explanation on the left, the control on the right at its natural width,
/// centred on the text block. Rows are divided by a thread line, except the
/// last.
///
/// The control is laid out first, so the text column gets exactly what is
/// left; the text block's height comes from the previous frame (it settles
/// after one frame) to centre the control on it.
#[allow(clippy::too_many_arguments)]
fn line(
    ui: &mut Ui,
    cx: &mut Cx,
    label: &str,
    draft: bool,
    hint: &str,
    last: bool,
    control: impl FnOnce(&mut Ui, &mut Cx),
) {
    const GAP: f32 = 12.0;
    const STACK_BELOW: f32 = 460.0;
    const MIN_TEXT_W: f32 = 200.0;
    ui.add_space(8.0);
    let key = ui.id().with(("settings_line_h", label));
    let text_h: f32 = ui.ctx().data(|d| d.get_temp(key)).unwrap_or(36.0);
    let full = ui.available_width();
    if full < STACK_BELOW {
        // Narrow pane (minimum window): the control goes under the text.
        line_text(ui, label, draft, hint, full);
        ui.add_space(8.0);
        ui.horizontal(|ui| control(ui, cx));
        ui.add_space(8.0);
        if !last {
            widgets::divider(ui);
        }
        return;
    }
    let top = ui.cursor().min;
    let ctrl_max = (full - MIN_TEXT_W - GAP).max(0.0);
    let ctrl_rect = Rect::from_min_max(
        pos2(top.x + full - ctrl_max, top.y),
        pos2(top.x + full, top.y + text_h),
    );
    let mut ctrl_ui = ui.new_child(
        UiBuilder::new()
            .max_rect(ctrl_rect)
            .layout(Layout::right_to_left(Align::Center)),
    );
    control(&mut ctrl_ui, cx);
    let used = ctrl_ui.min_rect().width();
    let text_w = (full - used - GAP).max(MIN_TEXT_W);
    let text_rect = line_text(ui, label, draft, hint, text_w);
    if (text_rect.height() - text_h).abs() > 0.5 {
        ui.ctx()
            .data_mut(|d| d.insert_temp(key, text_rect.height()));
        ui.ctx().request_repaint();
    }
    ui.add_space(8.0);
    if !last {
        widgets::divider(ui);
    }
}

/// The text block of a settings row: label (and Draft tag) over the hint.
fn line_text(ui: &mut Ui, label: &str, draft: bool, hint: &str, width: f32) -> Rect {
    ui.allocate_ui_with_layout(vec2(width, 0.0), Layout::top_down(Align::Min), |ui| {
        ui.set_width(width);
        // 22 px: the Draft tag's height.
        contract::row(ui, "setting-label", 22.0, |ui| {
            contract::text(
                ui,
                RichText::new(label)
                    .font(theme::bold(13.0))
                    .color(theme::INK),
                true,
            );
            if draft {
                widgets::tag(ui, "Draft", true);
            }
        });
        ui.add_space(2.0);
        ui.label(
            RichText::new(hint)
                .font(theme::body(11.5))
                .color(theme::INK_FAINT),
        );
    })
    .response
    .rect
}
