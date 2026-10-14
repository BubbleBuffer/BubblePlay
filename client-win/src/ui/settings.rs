//! Settings: this device's key (ID, key file, Export, Import, Replace key).
//! Mockup: client-win/mockups/connect.html state settings.
//!
//! Uses the flow layout (rail + pane) from flow.rs.

use egui::{vec2, Align, Layout, RichText, Ui};

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
            ui.set_max_width(620.0);
            let id = state.this_device.id.as_str();
            line(ui, cx, "Device ID", false, "PCs show this ID when they pair or list this device.", false, |ui, _| {
                widgets::id_text(ui, id, true);
            });
            line(
                ui,
                cx,
                "Key file",
                false,
                "Encrypted at rest (DPAPI). Copying the file to another PC or account does not work; use Export.",
                false,
                |ui, _| {
                    widgets::mono_text(ui, &state.this_device.key_path, 12.5, theme::INK_MUTED);
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
                if widgets::button(ui, cx, BtnKind::Ghost, BtnSize::Normal, None, "Back to your PCs").clicked() {
                    go_home = true;
                }
            });
        },
    );
    if back || go_home {
        us.view = View::Home(HomeSel::FirstPc);
    }
}

/// One row of the settings list: bold label (with an optional Draft tag),
/// a faint explanation under it, and the control on the right. Rows are
/// divided by a thread line, except the last.
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
    ui.add_space(8.0);
    // A row sized to its content. egui's `horizontal_top` claims all the
    // remaining height of the pane, which pushed the other rows out of view.
    let h = ui.spacing().interact_size.y;
    ui.allocate_ui_with_layout(vec2(ui.available_width(), h), Layout::left_to_right(Align::Min), |ui| {
        let text_rect = ui
            .vertical(|ui| {
                ui.set_max_width(400.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(label).font(theme::bold(13.0)).color(theme::INK));
                    if draft {
                        widgets::tag(ui, "Draft", true);
                    }
                });
                ui.label(RichText::new(hint).font(theme::body(11.5)).color(theme::INK_FAINT));
            })
            .response
            .rect;
        // The control goes at the right edge, centred on the text block.
        ui.allocate_ui_with_layout(
            vec2(ui.available_width(), text_rect.height()),
            Layout::right_to_left(Align::Center),
            |ui| control(ui, cx),
        );
    });
    ui.add_space(8.0);
    if !last {
        widgets::divider(ui);
    }
}
