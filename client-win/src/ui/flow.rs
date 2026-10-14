//! Flow screens: 300 px step rail + pane. First start (create or import a
//! device key), pairing at the PC or remotely (enter, compare, outcome),
//! before-sign-in approval, connecting, the ways a connect fails, key
//! refused, ID rotation. Mockup: client-win/mockups/connect.html.
//!
//! The rail and pane frame, title and action helpers are `pub(super)` so
//! settings.rs draws the same layout.

use std::time::Duration;

use egui::{
    pos2, vec2, Align, Align2, Color32, CornerRadius, Frame, Id, Layout, Margin, Rect, RichText, Sense, Stroke, TextEdit,
    Ui, UiBuilder,
};

use super::contract;
use super::theme;
use super::widgets::{self, BtnKind, BtnSize, NoticeKind};
use super::{Cx, UiState};
use crate::model::{
    Action, Answerer, AppState, ConnError, ConnStep, EnrollStep, Flow, Outcome, RotatePc, StepStatus, StreamMode,
};


pub fn show(ui: &mut Ui, cx: &mut Cx, state: &AppState, flow: &Flow, _us: &mut UiState) {
    let back = match flow {
        Flow::Enroll(step) => enroll(ui, cx, state, step),
        Flow::PairEnter { remote: false, error } => pair_enter(ui, cx, state, error.as_deref()),
        Flow::PairEnter { remote: true, error } => pair_remote(ui, cx, state, error.as_deref()),
        Flow::PairConfirm { pc_name, pc_id, number, waiting_secs, remote, outcome } => match outcome {
            Outcome::Waiting => pair_confirm(ui, cx, state, pc_name, pc_id, *number, *waiting_secs, *remote),
            _ => pair_result(ui, cx, state, pc_name, pc_id, *remote, outcome),
        },
        Flow::Approval { pc_id, waiting_secs, outcome } => match outcome {
            Outcome::Waiting | Outcome::Approved { .. } => approval(ui, cx, state, pc_id, *waiting_secs),
            _ => approval_result(ui, cx, state, pc_id, outcome),
        },
        Flow::Connecting { pc_id, mode, steps } => connecting(ui, cx, state, pc_id, *mode, steps),
        Flow::Failed { pc_id, steps, error } => failed(ui, cx, state, pc_id, steps, error),
        Flow::Refused { pc_id } => refused(ui, cx, state, pc_id),
        Flow::Rotate { old_id, new_id, pcs } => rotate(ui, cx, state, old_id, new_id, pcs),
    };
    if back {
        cx.actions.push(Action::CancelFlow);
    }
}

// ------------------------------------------------------------ first start

fn enroll(ui: &mut Ui, cx: &mut Cx, state: &AppState, step: &EnrollStep) -> bool {
    let (key_sub, key_status) = match step {
        EnrollStep::Choose => ("Create or import", StepStatus::Now),
        EnrollStep::Created => ("Created", StepStatus::Done),
        EnrollStep::ImportUnlock { .. } => ("Unlock the key file", StepStatus::Now),
        EnrollStep::ImportRefused { .. } => ("File not usable", StepStatus::Failed),
    };
    let pair_status = if *step == EnrollStep::Created { StepStatus::Now } else { StepStatus::Waiting };
    // No back link: there is nothing to go back to before a key exists.
    layout_ex(
        ui,
        cx,
        &state.this_device.id,
        false,
        |ui, cx| {
            rail_title(ui, "Set up");
            rail_text(ui, "BubblePlay for Windows");
            steps_block(
                ui,
                cx,
                &[
                    ("Device key", key_sub, key_status),
                    ("Pair a PC", "At the PC, or remotely", pair_status),
                    ("Connect", "Opens in Work mode", StepStatus::Waiting),
                ],
            );
        },
        |ui, cx| match step {
            EnrollStep::Choose => enroll_choose(ui, cx),
            EnrollStep::Created => enroll_created(ui, cx, state),
            EnrollStep::ImportUnlock { file_name, error } => enroll_unlock(ui, cx, file_name, error.as_deref()),
            EnrollStep::ImportRefused { file_name, reason } => enroll_refused(ui, cx, file_name, reason),
        },
    )
}

fn enroll_choose(ui: &mut Ui, cx: &mut Cx) {
    title_row(ui, "This device needs a key", None);
    lead(
        ui,
        "BubblePlay connects with device keys, not passwords. A key gives this device its own ID, like an SSH key, and stays on this Windows account.",
    );
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 12.0;
        choice(
            ui,
            cx,
            "key-round",
            theme::SIGNAL,
            "Create a new key",
            "Gives this device a new ID. You pair it with each PC once, and can export a backup afterwards.",
            |ui, cx| {
                if widgets::button(ui, cx, BtnKind::Primary, BtnSize::Normal, None, "Create key").clicked() {
                    cx.actions.push(Action::CreateKey);
                }
            },
        );
        choice(
            ui,
            cx,
            "file-key",
            theme::INK_MUTED,
            "Import a key file",
            "Use a .bpkey file exported from another device or an earlier install. PCs that know that key accept this device right away.",
            |ui, cx| {
                if widgets::button(ui, cx, BtnKind::Secondary, BtnSize::Normal, Some("file-up"), "Import key file…").clicked() {
                    cx.actions.push(Action::ImportKey);
                }
            },
        );
    });
    ui.add_space(20.0);
    widgets::notice(ui, cx, NoticeKind::Neutral, "info", |ui| {
        body_muted(ui, "One key per device is safer: a PC can then remove one device without cutting off the others.");
    });
}

fn enroll_created(ui: &mut Ui, cx: &mut Cx, state: &AppState) {
    title_row(ui, "Key created", None);
    lead(ui, "This is this device's ID. A PC shows it when you pair, so you can check it is this device.");
    card(ui, theme::SURFACE, theme::THREAD, theme::R_PANEL, Margin { left: 20, right: 20, top: 18, bottom: 18 }, |ui| {
        widgets::section_label(ui, "This device");
        ui.add_space(4.0);
        widgets::mono_text(ui, &state.this_device.id, 22.0, theme::INK);
        ui.add_space(6.0);
        contract::text(
            ui,
            RichText::new(&state.this_device.key_path).font(theme::mono(12.0)).color(theme::INK_FAINT),
            true,
        );
    });
    ui.add_space(18.0);
    widgets::notice(ui, cx, NoticeKind::Info, "download", |ui| {
        body_muted(
            ui,
            "Export a backup if you might reinstall Windows or move to a new PC. Without one, a reinstall is a new device that pairs again.",
        );
    });
    actions(ui, |ui| {
        if widgets::button(ui, cx, BtnKind::Primary, BtnSize::Large, Some("arrow-right"), "Pair a PC").clicked() {
            cx.actions.push(Action::StartPairing);
        }
        if widgets::button(ui, cx, BtnKind::Secondary, BtnSize::Normal, Some("download"), "Export backup…").clicked() {
            cx.actions.push(Action::ExportKey);
        }
        if widgets::button(ui, cx, BtnKind::Ghost, BtnSize::Normal, None, "Later").clicked() {
            cx.actions.push(Action::CancelFlow);
        }
    });
}

fn enroll_unlock(ui: &mut Ui, cx: &mut Cx, file_name: &str, error: Option<&str>) {
    title_row(ui, "Unlock the key file", None);
    lead(
        ui,
        &format!(
            "{file_name} is protected with a passphrase. Type it once: BubblePlay then keeps the key with Windows data protection and does not ask again."
        ),
    );
    if let Some(e) = error {
        error_notice(ui, cx, e);
        ui.add_space(18.0);
    }
    text_field(ui, cx, "passphrase", "Passphrase", Some("lock"), "", theme::body(14.0), true, |_, _| {});
    hint(ui, "Checked on this PC only. It is never sent to a relay or a PC.");
    let passphrase = stored(ui, "passphrase");
    actions(ui, |ui| {
        if passphrase.is_empty() {
            widgets::button_disabled(ui, cx, BtnKind::Primary, BtnSize::Large, Some("key-round"), "Unlock");
        } else if widgets::button(ui, cx, BtnKind::Primary, BtnSize::Large, Some("key-round"), "Unlock").clicked() {
            store(ui, "passphrase", String::new());
            cx.actions.push(Action::UnlockImport { passphrase });
        }
        if widgets::button(ui, cx, BtnKind::Secondary, BtnSize::Normal, Some("file-up"), "Choose another file").clicked() {
            store(ui, "passphrase", String::new());
            cx.actions.push(Action::ImportKey);
        }
        if widgets::button(ui, cx, BtnKind::Ghost, BtnSize::Normal, None, "Back").clicked() {
            store(ui, "passphrase", String::new());
            cx.actions.push(Action::CancelFlow);
        }
    });
}

fn enroll_refused(ui: &mut Ui, cx: &mut Cx, file_name: &str, reason: &str) {
    title_row(ui, "This file can't be used", None);
    lead_tight(ui, "BubblePlay checked the key file and stopped before keeping anything.");
    ui.add_space(20.0);
    widgets::notice(ui, cx, NoticeKind::Error, "shield-x", |ui| {
        ui.label(RichText::new(file_name).font(theme::mono(12.5)).color(theme::INK));
        ui.add_space(4.0);
        body_muted(ui, reason);
    });
    actions(ui, |ui| {
        if widgets::button(ui, cx, BtnKind::Primary, BtnSize::Large, Some("file-up"), "Choose another file").clicked() {
            cx.actions.push(Action::ImportKey);
        }
        if widgets::button(ui, cx, BtnKind::Secondary, BtnSize::Normal, Some("key-round"), "Create a new key").clicked() {
            cx.actions.push(Action::CreateKey);
        }
    });
}

/// One of the two first-start choices: an icon, a bold title, a short
/// paragraph and the choice's button, in a card.
fn choice(
    ui: &mut Ui,
    cx: &mut Cx,
    icon: &str,
    icon_color: Color32,
    title: &str,
    body: &str,
    button: impl FnOnce(&mut Ui, &mut Cx),
) {
    card(ui, theme::SURFACE, theme::THREAD, theme::R_PANEL, Margin { left: 18, right: 18, top: 16, bottom: 16 }, |ui| {
        top_row(ui, |ui| {
            widgets::icon(ui, cx, icon, theme::ICON_LG, icon_color);
            ui.add_space(14.0);
            ui.vertical(|ui| {
                ui.label(RichText::new(title).font(theme::bold(15.0)).color(theme::INK));
                ui.add_space(2.0);
                body_muted(ui, body);
                ui.add_space(12.0);
                button(ui, cx);
            });
        });
    });
}

// ------------------------------------------------------------ pairing

/// What the pairing inputs hold: the link, or PC ID, code and relay.
struct PairInput {
    link: String,
    pc_id: String,
    code: String,
    relay: String,
}

impl PairInput {
    fn read(ui: &mut Ui) -> Self {
        Self {
            link: stored(ui, "link").trim().to_string(),
            pc_id: normalize_id(&stored(ui, "pc-id")),
            code: stored(ui, "code").trim().to_string(),
            relay: stored(ui, "relay").trim().to_string(),
        }
    }

    fn link_ok(&self) -> bool {
        (self.link.starts_with("https://") || self.link.starts_with("bubbleplay://")) && self.link.contains("pair")
    }

    fn typed_ok(&self) -> bool {
        valid_id(&self.pc_id) && self.code.chars().filter(|c| c.is_ascii_alphanumeric()).count() == 10 && !self.relay.is_empty()
    }
}

fn pair_enter(ui: &mut Ui, cx: &mut Cx, state: &AppState, error: Option<&str>) -> bool {
    layout(
        ui,
        cx,
        &state.this_device.id,
        |ui, cx| {
            rail_title(ui, "Pair a new PC");
            rail_text(ui, "On the PC: BubblePlay tray icon, Pair device.");
            steps_block(
                ui,
                cx,
                &[
                    ("PC ID and code", "From the tray, valid 5 minutes", StepStatus::Now),
                    ("Compare the number", "Allowed at the PC or vouched", StepStatus::Waiting),
                    ("Done", "The PC joins Your PCs", StepStatus::Waiting),
                ],
            );
        },
        |ui, cx| {
            title_row(ui, "Enter what the PC shows", None);
            lead(
                ui,
                "Open the BubblePlay tray icon on the PC and choose Pair device. Type the PC ID and code, or paste the pairing link.",
            );
            if let Some(e) = error {
                error_notice(ui, cx, e);
                ui.add_space(18.0);
            }
            // Paste needs a clipboard action the backend does not offer yet,
            // so the button is shown disabled; Ctrl+V works in the box.
            text_field(
                ui,
                cx,
                "link",
                "Pairing link",
                Some("link"),
                "https://<relay>/pair#…",
                theme::body(14.0),
                false,
                |ui, cx| widgets::button_disabled(ui, cx, BtnKind::Ghost, BtnSize::Small, None, "Paste"),
            );
            ui.add_space(18.0);
            or_divider(ui, "or type them");
            ui.add_space(18.0);
            text_field(ui, cx, "pc-id", "PC ID", None, "XXXX-XXXX-XXXX-XXXX", theme::mono(15.0), false, |_, _| {});
            ui.add_space(18.0);
            text_field(ui, cx, "code", "One-time code", None, "", theme::mono(15.0), false, |_, _| {});
            hint(ui, "10 characters, shown under the QR. It works once and expires after 5 minutes.");
            ui.add_space(18.0);
            relay_field(ui, cx);
            let input = PairInput::read(ui);
            actions(ui, |ui| {
                let ready = input.link_ok() || input.typed_ok();
                if !ready {
                    widgets::button_disabled(ui, cx, BtnKind::Primary, BtnSize::Large, Some("arrow-right"), "Continue");
                } else if widgets::button(ui, cx, BtnKind::Primary, BtnSize::Large, Some("arrow-right"), "Continue").clicked() {
                    // The code works once: never keep it for a second try.
                    store(ui, "code", String::new());
                    let link = input.link_ok().then(|| input.link.clone());
                    cx.actions.push(Action::SubmitPairing {
                        link,
                        pc_id: input.pc_id.clone(),
                        code: input.code.clone(),
                        relay: input.relay.clone(),
                    });
                }
                if widgets::button(ui, cx, BtnKind::Ghost, BtnSize::Normal, None, "Cancel").clicked() {
                    cx.actions.push(Action::CancelFlow);
                }
            });
            switch_row(ui, cx, "Nobody at the PC?", "Ask for remote pairing", Action::StartRemotePairing);
        },
    )
}

fn pair_remote(ui: &mut Ui, cx: &mut Cx, state: &AppState, error: Option<&str>) -> bool {
    layout(
        ui,
        cx,
        &state.this_device.id,
        |ui, cx| {
            rail_title(ui, "Pair a new PC");
            rail_text(ui, "Remotely, without a code.");
            steps_block(
                ui,
                cx,
                &[
                    ("PC ID", "Remote pairing on at the PC", StepStatus::Now),
                    ("Compare the number", "Allowed at the PC or vouched", StepStatus::Waiting),
                    ("Done", "The PC joins Your PCs", StepStatus::Waiting),
                ],
            );
        },
        |ui, cx| {
            title_row(ui, "Ask a PC to pair remotely", None);
            lead(
                ui,
                "This works when Allow remote pairing is on at the PC. Someone then approves at the PC, or on a device already paired with it. A remote pairing gets view and input.",
            );
            if let Some(e) = error {
                error_notice(ui, cx, e);
                ui.add_space(18.0);
            }
            text_field(ui, cx, "pc-id", "PC ID", None, "XXXX-XXXX-XXXX-XXXX", theme::mono(15.0), false, |_, _| {});
            hint(ui, "In the tray on the PC, under Devices. A paired device shows it under the PC too.");
            ui.add_space(18.0);
            relay_field(ui, cx);
            ui.add_space(20.0);
            widgets::notice(ui, cx, NoticeKind::Neutral, "clock", |ui| {
                body_muted(ui, "A request expires after 10 minutes, and a PC takes one request at a time.");
            });
            let input = PairInput::read(ui);
            actions(ui, |ui| {
                let ready = valid_id(&input.pc_id) && !input.relay.is_empty();
                if !ready {
                    widgets::button_disabled(ui, cx, BtnKind::Primary, BtnSize::Large, Some("arrow-right"), "Send request");
                } else if widgets::button(ui, cx, BtnKind::Primary, BtnSize::Large, Some("arrow-right"), "Send request").clicked() {
                    cx.actions.push(Action::RequestRemotePairing { pc_id: input.pc_id.clone(), relay: input.relay.clone() });
                }
                if widgets::button(ui, cx, BtnKind::Ghost, BtnSize::Normal, None, "Cancel").clicked() {
                    cx.actions.push(Action::CancelFlow);
                }
            });
            switch_row(ui, cx, "At the PC?", "Use a code instead", Action::StartPairing);
        },
    )
}

fn relay_field(ui: &mut Ui, cx: &Cx) {
    text_field(
        ui,
        cx,
        "relay",
        "Relay address",
        Some("globe"),
        "play.bubblebuffer.net",
        theme::mono(13.0),
        false,
        |_, _| {},
    );
}

/// "Nobody at the PC? [Ask for remote pairing]": a quiet way over to the
/// other kind of pairing, under the pane's buttons.
fn switch_row(ui: &mut Ui, cx: &mut Cx, question: &str, label: &str, action: Action) {
    ui.add_space(22.0);
    widgets::divider(ui);
    ui.add_space(14.0);
    // One 32 px row, centred, so the hint sits on the button's centre line.
    ui.allocate_ui_with_layout(vec2(ui.available_width(), 32.0), Layout::left_to_right(Align::Center), |ui| {
        hint(ui, question);
        if widgets::button(ui, cx, BtnKind::Ghost, BtnSize::Small, None, label).clicked() {
            cx.actions.push(action);
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn pair_confirm(
    ui: &mut Ui,
    cx: &mut Cx,
    state: &AppState,
    pc_name: &str,
    pc_id: &str,
    number: u32,
    waiting_secs: u32,
    remote: bool,
) -> bool {
    let device_prefix = format!("{}-…", short(&state.this_device.id, 4));
    let first = first_pair_step(remote, StepStatus::Done);
    layout(
        ui,
        cx,
        &state.this_device.id,
        |ui, cx| {
            rail_title(ui, "Pair a new PC");
            rail_text(ui, if remote { "Remotely, without a code." } else { "On the PC: BubblePlay tray icon, Pair device." });
            steps_block(
                ui,
                cx,
                &[
                    first,
                    ("Compare the number", "Waiting for a yes", StepStatus::Now),
                    ("Done", "The PC joins Your PCs", StepStatus::Waiting),
                ],
            );
        },
        |ui, cx| {
            title_row(ui, "Check this number on the PC", None);
            let lead_text = if remote {
                "The PC shows the same number with the request. If it matches, allow it there, or vouch for this device on one already paired with it."
            } else {
                "The PC shows the same number. If it matches, allow it there. If nobody is at the PC, a device already paired with it can vouch for this one."
            };
            lead(ui, lead_text);
            card(
                ui,
                theme::SURFACE,
                theme::THREAD,
                theme::R_PANEL,
                Margin { left: 20, right: 20, top: 18, bottom: 18 },
                |ui| {
                    ui.label(RichText::new("Number on this device").font(theme::bold(12.0)).color(theme::INK_FAINT));
                    ui.add_space(6.0);
                    widgets::number(ui, number, 44.0);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("{pc_name} · ")).font(theme::body(12.0)).color(theme::INK_FAINT));
                        widgets::id_text(ui, pc_id, false);
                    });
                },
            );
            ways(ui, cx, |ui, cx| {
                let at_pc = if remote {
                    format!(
                        "The tray shows a pairing request from this device (ID {device_prefix}) with the number {}. Choose Allow.",
                        number_text(number)
                    )
                } else {
                    format!(
                        "The tray asks: Pair this device (ID {device_prefix})? Number on the device: {}. Choose Allow.",
                        number_text(number)
                    )
                };
                way(ui, cx, "monitor-check", "At the PC", &at_pc);
                way(
                    ui,
                    cx,
                    "user-check",
                    "On a paired device",
                    &format!(
                        "Open BubblePlay on a device that may vouch for {pc_name}. The request waits under {pc_name} for 10 minutes."
                    ),
                );
            });
            actions(ui, |ui| {
                if widgets::button(ui, cx, BtnKind::Secondary, BtnSize::Normal, None, "Cancel pairing").clicked() {
                    cx.actions.push(Action::CancelFlow);
                }
                widgets::mono_text(ui, &format!("Waiting {}", clock(waiting_secs)), 12.0, theme::INK_FAINT);
            });
        },
    )
}

fn first_pair_step(remote: bool, status: StepStatus) -> (&'static str, &'static str, StepStatus) {
    if remote {
        ("PC ID", "Request sent", status)
    } else {
        ("PC ID and code", "Code accepted", status)
    }
}

/// The end of a pairing wait: approved, denied, expired or closed at the PC.
fn pair_result(
    ui: &mut Ui,
    cx: &mut Cx,
    state: &AppState,
    pc_name: &str,
    pc_id: &str,
    remote: bool,
    outcome: &Outcome,
) -> bool {
    let answered = |by: &Answerer, yes: bool| match (by, yes) {
        (Answerer::Pc, true) => "Allowed at the PC".to_string(),
        (Answerer::Device(d), true) => format!("Vouched by {d}"),
        (Answerer::Pc, false) => "Denied at the PC".to_string(),
        (Answerer::Device(d), false) => format!("Denied on {d}"),
    };
    let (compare_sub, compare_status) = match outcome {
        Outcome::Approved { by } => (answered(by, true), StepStatus::Done),
        Outcome::Denied { by } => (answered(by, false), StepStatus::Failed),
        Outcome::TimedOut => ("Nobody answered".to_string(), StepStatus::Failed),
        Outcome::Cancelled | Outcome::Waiting => ("Closed at the PC".to_string(), StepStatus::Failed),
    };
    let approved = matches!(outcome, Outcome::Approved { .. });
    let done_sub = if approved { format!("{pc_name} is in Your PCs") } else { "Nothing was added".to_string() };
    let done_status = if approved { StepStatus::Done } else { StepStatus::Waiting };
    let pc = state.pcs.iter().find(|p| p.id == pc_id);
    layout(
        ui,
        cx,
        &state.this_device.id,
        |ui, cx| {
            rail_title(ui, "Pair a new PC");
            rail_text(ui, if remote { "Remotely, without a code." } else { "On the PC: BubblePlay tray icon, Pair device." });
            steps_block(
                ui,
                cx,
                &[
                    first_pair_step(remote, StepStatus::Done),
                    ("Compare the number", compare_sub.as_str(), compare_status),
                    ("Done", done_sub.as_str(), done_status),
                ],
            );
        },
        |ui, cx| {
            let again = if remote { Action::StartRemotePairing } else { Action::StartPairing };
            match outcome {
                Outcome::Approved { by } => {
                    result_tile(ui, cx, "circle-check", theme::CONSTRUCTIVE, theme::SURFACE);
                    title_row(ui, &format!("{pc_name} is paired"), None);
                    let why = match by {
                        Answerer::Pc => format!("Allowed at {pc_name}."),
                        Answerer::Device(d) => format!("{d} vouched for this device."),
                    };
                    lead(ui, &format!("{why} It is in Your PCs now, and this device is on its list."));
                    let relay = pc.map(|p| p.relay.as_str()).unwrap_or("");
                    let allowed = pc.map(|p| p.allowed.as_str()).unwrap_or("View and input");
                    card(ui, theme::SURFACE, theme::THREAD, theme::R_PANEL, Margin { left: 20, right: 20, top: 16, bottom: 16 }, |ui| {
                        egui::Grid::new("pair_result_facts").num_columns(2).spacing([20.0, 10.0]).show(ui, |ui| {
                            fact_key(ui, "PC ID");
                            widgets::id_text(ui, pc_id, false);
                            ui.end_row();
                            fact_key(ui, "Relay");
                            widgets::mono_text(ui, relay, 12.0, theme::INK);
                            ui.end_row();
                            fact_key(ui, "Allowed");
                            fact_text(ui, allowed);
                            ui.end_row();
                            fact_key(ui, "First mode");
                            fact_text(ui, "Work. Switch to Game from the session strip.");
                            ui.end_row();
                        });
                    });
                    actions(ui, |ui| {
                        if widgets::button(ui, cx, BtnKind::Primary, BtnSize::Large, Some("monitor-play"), &format!("Connect to {pc_name}"))
                            .clicked()
                        {
                            cx.actions.push(Action::Connect { pc_id: pc_id.to_string() });
                        }
                        if widgets::button(ui, cx, BtnKind::Secondary, BtnSize::Normal, None, "Back to your PCs").clicked() {
                            cx.actions.push(Action::CancelFlow);
                        }
                    });
                }
                _ => {
                    let (icon, color, title, text, tip) = match outcome {
                        Outcome::Denied { by } => (
                            "shield-x",
                            theme::DANGER,
                            "Pairing was denied".to_string(),
                            match by {
                                Answerer::Pc => format!("Someone at {pc_name} chose Deny. Nothing was added on either side."),
                                Answerer::Device(d) => {
                                    format!("{d} chose Deny instead of vouching for this device. Nothing was added on either side.")
                                }
                            },
                            if remote {
                                "If that was a mistake, send a new request.".to_string()
                            } else {
                                "If that was a mistake, choose Pair device in the tray again for a new code.".to_string()
                            },
                        ),
                        Outcome::TimedOut => (
                            "hourglass",
                            theme::ATTENTION,
                            "The request expired".to_string(),
                            if remote {
                                "Nobody approved it within 10 minutes. Nothing was added.".to_string()
                            } else {
                                format!("Nobody answered at {pc_name} within 5 minutes, so the code stopped working. Nothing was added.")
                            },
                            if remote {
                                format!("Ask someone to be at {pc_name} or on a paired device, then send a new request.")
                            } else {
                                "For a new code, choose Pair device in the tray on the PC.".to_string()
                            },
                        ),
                        _ => (
                            "ban",
                            theme::INK_MUTED,
                            "Pairing was closed at the PC".to_string(),
                            format!("The pairing window on {pc_name} closed before anyone answered. Nothing was added."),
                            if remote {
                                "Send a new request when someone can answer it.".to_string()
                            } else {
                                "Choose Pair device in the tray again for a new code.".to_string()
                            },
                        ),
                    };
                    result_tile(ui, cx, icon, color, theme::SURFACE);
                    title_row(ui, &title, None);
                    lead(ui, &text);
                    widgets::notice(ui, cx, NoticeKind::Neutral, "info", |ui| body_muted(ui, &tip));
                    actions(ui, |ui| {
                        if widgets::button(ui, cx, BtnKind::Primary, BtnSize::Large, Some("refresh-cw"), "Start again").clicked() {
                            cx.actions.push(again);
                        }
                        if widgets::button(ui, cx, BtnKind::Secondary, BtnSize::Normal, None, "Back to your PCs").clicked() {
                            cx.actions.push(Action::CancelFlow);
                        }
                    });
                }
            }
        },
    )
}

// ------------------------------------------------------------ before sign-in

fn approval(ui: &mut Ui, cx: &mut Cx, state: &AppState, pc_id: &str, waiting_secs: u32) -> bool {
    let pc = pc_name(state, pc_id);
    let accepted = format!("Accepted by {pc}");
    layout(
        ui,
        cx,
        &state.this_device.id,
        |ui, cx| {
            rail_title(ui, &pc);
            widgets::id_text(ui, pc_id, false);
            steps_block(
                ui,
                cx,
                &[
                    ("Device key", accepted.as_str(), StepStatus::Done),
                    ("Before sign-in approval", "Needed every time", StepStatus::Now),
                    ("Stream", "Sign-in screen", StepStatus::Waiting),
                ],
            );
        },
        |ui, cx| {
            title_row(ui, "PC is at the sign-in screen", None);
            lead(
                ui,
                "Connecting before Windows sign-in needs a fresh approval every time, even for a paired device.",
            );
            top_row(ui, |ui| {
                pulse(ui, cx);
                ui.add_space(20.0);
                ui.vertical(|ui| {
                    ui.label(RichText::new("Waiting for approval").font(theme::bold(15.0)).color(theme::INK));
                    widgets::mono_text(ui, &format!("Waiting {}", clock(waiting_secs)), 12.0, theme::INK_FAINT);
                    ways(ui, cx, |ui, cx| {
                        way(
                            ui,
                            cx,
                            "smartphone",
                            "On your phone",
                            &format!("{pc} itself sent an ntfy notification for this connection. Tap Approve."),
                        );
                    });
                });
            });
            ui.add_space(20.0);
            widgets::notice(ui, cx, NoticeKind::Neutral, "info", |ui| {
                ui.label(
                    RichText::new(
                        "Once you are in, you type the Windows password on the sign-in screen. The PC will note that someone connected before sign-in.",
                    )
                    .font(theme::body(13.0))
                    .color(theme::INK_MUTED),
                );
            });
            actions(ui, |ui| {
                if widgets::button(ui, cx, BtnKind::Secondary, BtnSize::Normal, None, "Cancel request").clicked() {
                    cx.actions.push(Action::CancelFlow);
                }
                hint(ui, "Moves on by itself once approved.");
            });
        },
    )
}

/// A before-sign-in approval that was denied, expired or withdrawn.
fn approval_result(ui: &mut Ui, cx: &mut Cx, state: &AppState, pc_id: &str, outcome: &Outcome) -> bool {
    let pc = pc_name(state, pc_id);
    let accepted = format!("Accepted by {pc}");
    let (icon, color, sub, title, text) = match outcome {
        Outcome::Denied { .. } => (
            "shield-x",
            theme::DANGER,
            "Denied",
            "The connection wasn't approved",
            format!("The notification from {pc} was answered with Deny. {pc} stays at its sign-in screen, and nothing was sent."),
        ),
        Outcome::TimedOut => (
            "hourglass",
            theme::ATTENTION,
            "No answer",
            "The approval expired",
            format!("Nobody approved the notification from {pc} in time. {pc} stays at its sign-in screen, and nothing was sent."),
        ),
        _ => (
            "ban",
            theme::INK_MUTED,
            "Withdrawn at the PC",
            "The request was closed at the PC",
            format!("{pc} withdrew the approval request before anyone answered. Nothing was sent."),
        ),
    };
    layout(
        ui,
        cx,
        &state.this_device.id,
        |ui, cx| {
            rail_title(ui, &pc);
            widgets::id_text(ui, pc_id, false);
            steps_block(
                ui,
                cx,
                &[
                    ("Device key", accepted.as_str(), StepStatus::Done),
                    ("Before sign-in approval", sub, StepStatus::Failed),
                    ("Stream", "Not started", StepStatus::Waiting),
                ],
            );
        },
        |ui, cx| {
            result_tile(ui, cx, icon, color, theme::SURFACE);
            title_row(ui, title, None);
            lead(ui, &text);
            widgets::notice(ui, cx, NoticeKind::Neutral, "smartphone", |ui| {
                body_muted(ui, &format!("Asking again sends a new notification from {pc}. Each one works once."));
            });
            actions(ui, |ui| {
                if widgets::button(ui, cx, BtnKind::Primary, BtnSize::Large, Some("refresh-cw"), "Ask again").clicked() {
                    cx.actions.push(Action::Retry);
                }
                if widgets::button(ui, cx, BtnKind::Secondary, BtnSize::Normal, None, "Back to your PCs").clicked() {
                    cx.actions.push(Action::CancelFlow);
                }
            });
        },
    )
}

// ------------------------------------------------------------ connecting

fn connecting(ui: &mut Ui, cx: &mut Cx, state: &AppState, pc_id: &str, mode: StreamMode, steps: &[ConnStep]) -> bool {
    let pc = pc_name(state, pc_id);
    let mode_text = mode_name(mode);
    let accepted = format!("Accepted by {pc}");
    // A PC never connected to has no last mode: it opens in Work.
    let first_time = state.pcs.iter().find(|p| p.id == pc_id).is_some_and(|p| p.last_mode.is_none());
    let mode_line = if first_time { format!("Mode: {mode_text} (new PC)") } else { format!("Last mode: {mode_text}") };
    let mode_lead = if first_time {
        format!("{mode_text} mode, the start for a new PC. Change it from the strip once you are in.")
    } else {
        format!("{mode_text} mode, as last time. Change it from the strip once you are in.")
    };
    layout(
        ui,
        cx,
        &state.this_device.id,
        |ui, cx| {
            rail_title(ui, &pc);
            widgets::id_text(ui, pc_id, false);
            steps_block(
                ui,
                cx,
                &[("Device key", accepted.as_str(), StepStatus::Done), ("Stream", mode_line.as_str(), StepStatus::Now)],
            );
        },
        |ui, cx| {
            title_row(ui, &format!("Connecting to {pc}"), None);
            lead_tight(ui, &mode_lead);
            ui.add_space(12.0);
            widgets::conn_steps(ui, cx, steps);
            indeterminate_bar(ui);
            actions(ui, |ui| {
                if widgets::button(ui, cx, BtnKind::Secondary, BtnSize::Normal, None, "Cancel").clicked() {
                    cx.actions.push(Action::CancelFlow);
                }
            });
        },
    )
}

/// The rail steps, title and lead for each way a connect can fail.
struct FailCopy {
    rail: Vec<(String, String, StepStatus)>,
    title: String,
    lead: String,
}

fn fail_copy(pc: &str, relay: &str, error: &ConnError) -> FailCopy {
    let key_done = || ("Device key".to_string(), format!("Accepted by {pc}"), StepStatus::Done);
    let s = |a: &str, b: &str, st| (a.to_string(), b.to_string(), st);
    match error {
        ConnError::NoFirstFrame { secs } => FailCopy {
            rail: vec![key_done(), s("Stream", "No first frame", StepStatus::Failed)],
            title: "The stream didn't start".into(),
            lead: format!("{pc} accepted this device, but no video arrived within {secs} seconds."),
        },
        ConnError::RelayUnreachable => FailCopy {
            rail: vec![
                s("Relay", "No answer", StepStatus::Failed),
                s("Device key", "Not checked", StepStatus::Waiting),
                s("Stream", "Not started", StepStatus::Waiting),
            ],
            title: "Can't reach the relay".into(),
            lead: format!("{relay} didn't answer. {pc} may be fine; this computer can't get to its relay right now."),
        },
        ConnError::PcBusy => FailCopy {
            rail: vec![key_done(), s("Stream", "PC busy", StepStatus::Failed)],
            title: format!("{pc} is in use"),
            lead: format!("Another device is connected to {pc}, and a PC takes one viewer at a time. Try again once it has left."),
        },
        ConnError::Lost { attempts } => FailCopy {
            rail: vec![key_done(), s("Stream", "Connection lost", StepStatus::Failed)],
            title: format!("Lost the connection to {pc}"),
            lead: format!("The session dropped and {attempts} reconnect attempts didn't bring it back."),
        },
        ConnError::NoHardwareDecoder { codec, .. } => FailCopy {
            rail: vec![key_done(), s("Stream", "No video decoder", StepStatus::Failed)],
            title: "This computer can't decode the stream".into(),
            lead: format!(
                "BubblePlay needs a hardware video decoder for {codec} and found none on this computer's graphics adapter. This happens in virtual machines, over Remote Desktop and with some drivers."
            ),
        },
        ConnError::PcKeyChanged => FailCopy {
            rail: vec![s("PC key", "Doesn't match", StepStatus::Failed), s("Stream", "Not started", StepStatus::Waiting)],
            title: format!("{pc}'s key changed"),
            lead: format!(
                "{pc} answered with a key this device doesn't know, and no signed rotation from its old key. BubblePlay stopped before sending anything."
            ),
        },
    }
}

fn failed(ui: &mut Ui, cx: &mut Cx, state: &AppState, pc_id: &str, steps: &[ConnStep], error: &ConnError) -> bool {
    let pc = pc_name(state, pc_id);
    let relay = state.pcs.iter().find(|p| p.id == pc_id).map(|p| p.relay.clone()).unwrap_or_default();
    let copy = fail_copy(&pc, &relay, error);
    layout(
        ui,
        cx,
        &state.this_device.id,
        |ui, cx| {
            rail_title(ui, &pc);
            widgets::id_text(ui, pc_id, false);
            let rail: Vec<(&str, &str, StepStatus)> = copy.rail.iter().map(|(a, b, s)| (a.as_str(), b.as_str(), *s)).collect();
            steps_block(ui, cx, &rail);
        },
        |ui, cx| {
            title_row(ui, &copy.title, None);
            lead_tight(ui, &copy.lead);
            ui.add_space(12.0);
            widgets::conn_steps(ui, cx, steps);
            let back = |ui: &mut Ui, cx: &mut Cx, kind: BtnKind| {
                if widgets::button(ui, cx, kind, BtnSize::Normal, None, "Back to your PCs").clicked() {
                    cx.actions.push(Action::CancelFlow);
                }
            };
            let try_again = |ui: &mut Ui, cx: &mut Cx, label: &str| {
                if widgets::button(ui, cx, BtnKind::Primary, BtnSize::Large, Some("refresh-cw"), label).clicked() {
                    cx.actions.push(Action::Retry);
                }
            };
            let (tip_kind, tip_icon, tip): (NoticeKind, &str, String) = match error {
                ConnError::NoFirstFrame { .. } => {
                    actions(ui, |ui| {
                        try_again(ui, cx, "Try again");
                        if widgets::button(ui, cx, BtnKind::Secondary, BtnSize::Normal, None, "Try again over WebSocket").clicked() {
                            cx.actions.push(Action::RetryWebSocket);
                        }
                        back(ui, cx, BtnKind::Ghost);
                    });
                    (
                        NoticeKind::Info,
                        "info",
                        "If the PC is asleep or its display is off, it may not send video. Wake it, then try again.".into(),
                    )
                }
                ConnError::RelayUnreachable => {
                    actions(ui, |ui| {
                        try_again(ui, cx, "Try again");
                        back(ui, cx, BtnKind::Secondary);
                    });
                    (
                        NoticeKind::Info,
                        "wifi-off",
                        "Check this computer's internet connection. If other PCs on the same relay fail too, the relay itself is down.".into(),
                    )
                }
                ConnError::PcBusy => {
                    actions(ui, |ui| {
                        try_again(ui, cx, "Try again");
                        back(ui, cx, BtnKind::Secondary);
                    });
                    (
                        NoticeKind::Neutral,
                        "info",
                        format!("Nothing changed on {pc}: the other session keeps running."),
                    )
                }
                ConnError::Lost { .. } => {
                    actions(ui, |ui| {
                        try_again(ui, cx, "Reconnect");
                        back(ui, cx, BtnKind::Secondary);
                    });
                    (
                        NoticeKind::Info,
                        "unplug",
                        format!("If {pc} went to sleep or lost its network, Your PCs shows it as offline until it is back."),
                    )
                }
                ConnError::NoHardwareDecoder { detail, .. } => {
                    ui.add_space(18.0);
                    widgets::notice(ui, cx, NoticeKind::Error, "cpu", |ui| {
                        ui.label(RichText::new("What Windows reported").font(theme::bold(13.0)).color(theme::INK));
                        ui.add_space(4.0);
                        ui.label(RichText::new(detail.as_str()).font(theme::mono(12.0)).color(theme::INK_MUTED));
                    });
                    actions(ui, |ui| {
                        back(ui, cx, BtnKind::Secondary);
                        if widgets::button(ui, cx, BtnKind::Ghost, BtnSize::Normal, Some("copy"), "Copy details").clicked() {
                            ui.ctx().copy_text(detail.clone());
                        }
                    });
                    (
                        NoticeKind::Info,
                        "info",
                        "Updating the graphics driver can help. This version has no software decoder, so a computer without hardware decoding can't show the stream yet.".into(),
                    )
                }
                ConnError::PcKeyChanged => {
                    ui.add_space(18.0);
                    widgets::notice(ui, cx, NoticeKind::Error, "shield-alert", |ui| {
                        body_muted(
                            ui,
                            &format!(
                                "This can mean {pc} was reinstalled without its old key, or that something is pretending to be {pc}. Pair again only after someone at the PC confirms the change."
                            ),
                        );
                    });
                    actions(ui, |ui| {
                        back(ui, cx, BtnKind::Secondary);
                        if widgets::button(ui, cx, BtnKind::Danger, BtnSize::Normal, Some("trash-2"), &format!("Forget {pc}"))
                            .clicked()
                        {
                            cx.actions.push(Action::Forget { pc_id: pc_id.to_string() });
                        }
                    });
                    (
                        NoticeKind::Neutral,
                        "info",
                        format!("Forgetting {pc} removes its pinned key here. Then pair it again from the tray on the PC."),
                    )
                }
            };
            ui.add_space(22.0);
            widgets::notice(ui, cx, tip_kind, tip_icon, |ui| body_muted(ui, &tip));
        },
    )
}

fn refused(ui: &mut Ui, cx: &mut Cx, state: &AppState, pc_id: &str) -> bool {
    let pc = pc_name(state, pc_id);
    let relay = state.pcs.iter().find(|p| p.id == pc_id).map(|p| p.relay.clone()).unwrap_or_default();
    let key = format!("{}-…", short(&state.this_device.id, 9));
    layout(
        ui,
        cx,
        &state.this_device.id,
        |ui, cx| {
            rail_title(ui, &pc);
            widgets::id_text(ui, pc_id, false);
            steps_block(
                ui,
                cx,
                &[("Device key", "Not accepted", StepStatus::Failed), ("Stream", "Not started", StepStatus::Waiting)],
            );
        },
        |ui, cx| {
            title_row(ui, &format!("{pc} didn't accept this device"), None);
            lead(
                ui,
                &format!("Its key is not on {pc}'s list. Either it was removed at the PC, or this device was never paired with it."),
            );
            refused_row(ui, cx, "circle-check", theme::CONSTRUCTIVE, "Relay reached", &relay, false);
            refused_row(ui, cx, "shield-x", theme::DANGER, "Device key not accepted", &key, true);
            ui.add_space(18.0);
            widgets::notice(ui, cx, NoticeKind::Neutral, "info", |ui| {
                ui.label(
                    RichText::new(format!(
                        "Your other PCs are not affected. To use {pc} again, pair it from the tray on the PC."
                    ))
                    .font(theme::body(13.0))
                    .color(theme::INK_MUTED),
                );
            });
            actions(ui, |ui| {
                if widgets::button(
                    ui,
                    cx,
                    BtnKind::Primary,
                    BtnSize::Large,
                    Some("key-round"),
                    &format!("Pair {pc} again"),
                )
                .clicked()
                {
                    cx.actions.push(Action::StartPairing);
                }
                if widgets::button(ui, cx, BtnKind::Danger, BtnSize::Normal, Some("trash-2"), &format!("Forget {pc}"))
                    .clicked()
                {
                    cx.actions.push(Action::Forget { pc_id: pc_id.to_string() });
                }
            });
            ui.add_space(22.0);
            widgets::notice(ui, cx, NoticeKind::Error, "shield-alert", |ui| {
                // One paragraph with a bold lead-in run: two labels in a
                // wrapped row broke the second one onto its own block.
                let mut job = egui::text::LayoutJob::default();
                let fmt = |font: egui::FontId, color| egui::TextFormat { font_id: font, color, ..Default::default() };
                job.append("Different case: PC key changed.", 0.0, fmt(theme::bold(13.0), theme::INK));
                job.append(
                    &format!(
                        " If {pc} answers with a key BubblePlay doesn't know and no signed rotation, the connection stops before anything is sent. Only pair again if someone at the PC confirms they changed it."
                    ),
                    0.0,
                    fmt(theme::body(13.0), theme::INK_MUTED),
                );
                ui.label(job);
            });
        },
    )
}

/// Card width below which the old and new IDs stack.
const ROTATE_SIDE_BY_SIDE_W: f32 = 380.0;

fn rotate(ui: &mut Ui, cx: &mut Cx, state: &AppState, old_id: &str, new_id: &str, pcs: &[RotatePc]) -> bool {
    let total = pcs.len();
    let confirmed = pcs.iter().filter(|p| p.status == StepStatus::Done).count();
    let new_prefix = format!("{}-…", short(new_id, 4));
    let progress = format!("{confirmed} of {total} so far");
    let conn: Vec<ConnStep> = pcs
        .iter()
        .map(|p| ConnStep { label: p.name.clone(), detail: p.detail.clone(), status: p.status })
        .collect();
    layout(
        ui,
        cx,
        &state.this_device.id,
        |ui, cx| {
            rail_title(ui, "This device");
            rail_text(ui, "Replace this device's key");
            steps_block(
                ui,
                cx,
                &[
                    ("New key created", new_prefix.as_str(), StepStatus::Done),
                    ("Each PC confirms", progress.as_str(), StepStatus::Now),
                    ("Old key removed", "After the last PC confirms", StepStatus::Waiting),
                ],
            );
        },
        |ui, cx| {
            title_row(ui, "Replacing this device's key", Some(("Draft copy", true)));
            lead(
                ui,
                "Each PC moves this device's name, permissions and history to the new ID. The old key stays until every PC has confirmed.",
            );
            card(
                ui,
                theme::SURFACE,
                theme::THREAD,
                theme::R_PANEL,
                Margin { left: 18, right: 18, top: 16, bottom: 16 },
                |ui| {
                    let old = |ui: &mut Ui| {
                        ui.vertical(|ui| {
                            widgets::section_label(ui, "Old ID");
                            ui.label(
                                RichText::new(old_id)
                                    .font(theme::mono(12.0))
                                    .color(theme::INK_FAINT)
                                    .strikethrough(),
                            );
                        });
                    };
                    let new = |ui: &mut Ui| {
                        ui.vertical(|ui| {
                            widgets::section_label(ui, "New ID");
                            widgets::id_text(ui, new_id, true);
                        });
                    };
                    // Old -> New side by side needs about 360 px; a narrower
                    // card stacks them (no down arrow in the icon set).
                    if ui.available_width() >= ROTATE_SIDE_BY_SIDE_W {
                        ui.horizontal(|ui| {
                            old(ui);
                            ui.add_space(14.0);
                            widgets::icon(ui, cx, "arrow-right", theme::ICON, theme::INK_FAINT);
                            ui.add_space(14.0);
                            new(ui);
                        });
                    } else {
                        old(ui);
                        ui.add_space(10.0);
                        new(ui);
                    }
                },
            );
            ui.add_space(16.0);
            widgets::conn_steps(ui, cx, &conn);
            ui.add_space(14.0);
            widgets::notice(ui, cx, NoticeKind::Neutral, "clock", |ui| {
                ui.vertical(|ui| {
                    widgets::tag(ui, "Draft timing", true);
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(
                            "No deadline: the old key is kept as long as a PC has not confirmed. After 30 days with a PC still waiting, Your PCs asks: keep waiting, or forget that PC and remove the old key now.",
                        )
                        .font(theme::body(13.0))
                        .color(theme::INK_MUTED),
                    );
                });
            });
            ui.add_space(18.0);
            widgets::notice(ui, cx, NoticeKind::Info, "info", |ui| {
                ui.vertical(|ui| {
                    widgets::tag(ui, "Draft", true);
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(
                            "You can keep connecting while this finishes. A copy of the old key file stops working once every PC has confirmed. Think the key leaked? Remove this device at each PC too, which also removes the new ID.",
                        )
                        .font(theme::body(13.0))
                        .color(theme::INK_MUTED),
                    );
                });
            });
            actions(ui, |ui| {
                if widgets::button(ui, cx, BtnKind::Secondary, BtnSize::Normal, None, "Back to your PCs").clicked() {
                    cx.actions.push(Action::CancelFlow);
                }
            });
        },
    )
}

// ------------------------------------------------------------ shared layout

/// Two columns: the rail (back link, title, sub, steps, footer) and the
/// pane. Returns true when the rail's back link was clicked; the caller
/// decides where that goes.
pub(super) fn layout(
    ui: &mut Ui,
    cx: &mut Cx,
    device_id: &str,
    rail: impl FnOnce(&mut Ui, &mut Cx),
    pane: impl FnOnce(&mut Ui, &mut Cx),
) -> bool {
    layout_ex(ui, cx, device_id, true, rail, pane)
}

/// [`layout`], with the rail's back link optional (first start has nowhere
/// to go back to).
fn layout_ex(
    ui: &mut Ui,
    cx: &mut Cx,
    device_id: &str,
    with_back: bool,
    rail: impl FnOnce(&mut Ui, &mut Cx),
    pane: impl FnOnce(&mut Ui, &mut Cx),
) -> bool {
    let mut back = false;
    let rail_rect = egui::Panel::left("flow_rail")
        .exact_size(theme::RAIL_W)
        // The rail/pane divider is drawn below in THREAD; egui's own panel
        // separator (which can turn white on hover) is switched off.
        .show_separator_line(false)
        .frame(Frame::NONE.fill(theme::RAIL).inner_margin(Margin { left: 28, right: 28, top: 32, bottom: 28 }))
        .show(ui, |ui| {
            let short = ui.available_height() < 520.0;
            if with_back {
                back = widgets::button(ui, cx, BtnKind::Ghost, BtnSize::Small, Some("chevron-left"), "Your PCs").clicked();
                ui.add_space(if short { 16.0 } else { 30.0 });
            } else {
                // Where the back link sits on the other screens, so titles
                // line up between screens.
                ui.add_space(if short { 8.0 } else { 18.0 });
            }
            // The rail body scrolls when the window is too short for it, so
            // the device footer stays inside the window.
            let body_h = (ui.available_height() - 52.0).max(0.0);
            contract::scroll(
                ui,
                "rail-body",
                egui::ScrollArea::vertical().max_height(body_h).auto_shrink([false, true]),
                |ui| rail(ui, cx),
            );
            let spare = ui.available_height() - 44.0;
            if spare > 0.0 {
                ui.add_space(spare);
            }
            ui.label(RichText::new("This device").font(theme::body(12.0)).color(theme::INK_FAINT));
            if device_id.is_empty() {
                ui.label(RichText::new("No key yet").font(theme::body(13.0)).color(theme::INK_MUTED));
            } else {
                widgets::id_text(ui, device_id, false);
            }
            contract::mark(ui.ctx(), ui.unique_id(), contract::BOX, "rail", ui.max_rect());
        })
        .response
        .rect;
    ui.painter().vline(rail_rect.right(), rail_rect.y_range(), Stroke::new(1.0, theme::THREAD));

    Frame::NONE.inner_margin(Margin { left: 52, right: 56, top: 38, bottom: 30 }).show(ui, |ui| {
        // The viewport width, measured outside the scroll area: inside it the
        // content ui keeps the previous frame's widest item as its width (and
        // its clip rect is wider than the viewport), so the pane is pinned to
        // this width in an explicit child ui; otherwise content runs past the
        // window at the minimum size.
        // Bounded by the window too: the parent ui can be wider than it.
        let window_w = ui.ctx().content_rect().right() - 56.0 - ui.max_rect().left();
        let w = (ui.available_width().min(window_w) - ui.spacing().scroll.allocated_width()).min(theme::PANE_MAX_W);
        contract::scroll(ui, "pane", egui::ScrollArea::vertical().auto_shrink([false, false]), |ui| {
            let top = ui.max_rect().min;
            ui.scope_builder(UiBuilder::new().max_rect(Rect::from_min_size(top, vec2(w, f32::INFINITY))), |ui| {
                ui.set_max_width(w);
                pane(ui, cx);
            });
        });
    });
    back
}

pub(super) fn rail_title(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).font(theme::display(30.0)).color(theme::INK));
    ui.add_space(8.0);
}

pub(super) fn rail_text(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).font(theme::body(13.0)).color(theme::INK_FAINT));
}

fn steps_block(ui: &mut Ui, cx: &Cx, steps: &[(&str, &str, StepStatus)]) {
    ui.add_space(30.0);
    widgets::rail_steps(ui, cx, steps);
}

/// Pane title with an optional tag beside it (Draft, Temporary).
pub(super) fn title_row(ui: &mut Ui, text: &str, tag: Option<(&str, bool)>) {
    let title = RichText::new(text).font(theme::display(theme::size::TITLE)).color(theme::INK);
    let title_w = ui
        .painter()
        .layout_no_wrap(text.to_owned(), theme::display(theme::size::TITLE), theme::INK)
        .size()
        .x;
    let tag_w = tag.map(|(t, _)| widgets::tag_width(ui, t) + ui.spacing().item_spacing.x).unwrap_or(0.0);
    if title_w + tag_w <= ui.available_width() {
        ui.horizontal(|ui| {
            ui.label(title);
            if let Some((t, draft)) = tag {
                widgets::tag(ui, t, draft);
            }
        });
    } else {
        // Narrow pane: the title wraps and the tag goes under it.
        ui.add(egui::Label::new(title).wrap());
        if let Some((t, draft)) = tag {
            widgets::tag(ui, t, draft);
        }
    }
    ui.add_space(6.0);
}

/// Lead paragraph with the mockup's 24 px gap below it.
pub(super) fn lead(ui: &mut Ui, text: &str) {
    lead_tight(ui, text);
    ui.add_space(24.0);
}

/// Lead paragraph with no gap below (the content follows directly).
pub(super) fn lead_tight(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).font(theme::body(theme::size::BODY)).color(theme::INK_MUTED));
}

/// The button row under a pane: 26 px above, 10 px between buttons. In a
/// narrow pane the buttons wrap onto a second line, 10 px below.
pub(super) fn actions(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    ui.add_space(26.0);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(10.0, 10.0);
        add(ui);
    });
}

// ------------------------------------------------------------ parts

/// A row that sizes to its content with children top-aligned (see the
/// same-named helper in widgets.rs for why not `horizontal_top`).
fn top_row(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    let h = ui.spacing().interact_size.y;
    ui.allocate_ui_with_layout(vec2(ui.available_width(), h), Layout::left_to_right(Align::Min), add);
}

fn card(ui: &mut Ui, fill: Color32, border: Color32, radius: CornerRadius, pad: Margin, add: impl FnOnce(&mut Ui)) {
    let frame = Frame::NONE.fill(fill).stroke(Stroke::new(1.0, border)).corner_radius(radius).inner_margin(pad);
    contract::framed(ui, "card", frame, |ui| {
        ui.set_width(ui.available_width());
        add(ui);
    });
}

fn way(ui: &mut Ui, cx: &Cx, icon: &str, title: &str, body: &str) {
    card(
        ui,
        theme::SURFACE_RAISED,
        theme::THREAD,
        theme::R_FIELD,
        Margin { left: 14, right: 14, top: 12, bottom: 12 },
        |ui| {
            top_row(ui, |ui| {
                widgets::icon(ui, cx, icon, theme::ICON, theme::INK_MUTED);
                ui.add_space(12.0);
                ui.vertical(|ui| {
                    ui.label(RichText::new(title).font(theme::bold(13.0)).color(theme::INK));
                    ui.label(RichText::new(body).font(theme::body(13.0)).color(theme::INK_MUTED));
                });
            });
        },
    );
}

fn ways(ui: &mut Ui, cx: &mut Cx, add: impl FnOnce(&mut Ui, &mut Cx)) {
    ui.add_space(16.0);
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 10.0;
        add(ui, cx);
    });
}

fn or_divider(ui: &mut Ui, text: &str) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 16.0), Sense::hover());
    let p = ui.painter();
    let line = Stroke::new(1.0, theme::THREAD);
    let tr = p.text(rect.center(), Align2::CENTER_CENTER, text, theme::body(12.0), theme::INK_FAINT);
    let y = rect.center().y;
    p.hline(rect.left()..=(tr.left() - 12.0), y, line);
    p.hline((tr.right() + 12.0)..=rect.right(), y, line);
}

fn hint(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).font(theme::body(12.0)).color(theme::INK_FAINT));
}

/// A 13 px paragraph in muted ink: notice bodies and card text.
fn body_muted(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).font(theme::body(13.0)).color(theme::INK_MUTED));
}

/// The 56 px tile above an outcome's title: one icon in the outcome's
/// colour on a quiet square.
fn result_tile(ui: &mut Ui, cx: &Cx, icon: &str, color: Color32, fill: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(56.0, 56.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, theme::R_PANEL, fill);
    p.rect_stroke(rect, theme::R_PANEL, Stroke::new(1.0, theme::THREAD), egui::StrokeKind::Inside);
    cx.icons.paint(p, Rect::from_center_size(rect.center(), vec2(24.0, 24.0)), icon, color);
    ui.add_space(18.0);
}

fn fact_key(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).font(theme::body(13.0)).color(theme::INK_FAINT));
}

fn fact_text(ui: &mut Ui, text: &str) {
    ui.add(egui::Label::new(RichText::new(text).font(theme::body(13.0)).color(theme::INK)).wrap());
}

fn error_notice(ui: &mut Ui, cx: &Cx, text: &str) {
    widgets::notice(ui, cx, NoticeKind::Error, "shield-alert", |ui| {
        ui.label(RichText::new(text).font(theme::body(13.0)).color(theme::INK_MUTED));
    });
}

/// A labelled one-line text box whose value lives in egui's temporary memory
/// (not saved to disk). The icon is drawn once, by `widgets::field`. `trailing`
/// draws an optional item (e.g. Paste) right-aligned inside the box; the text
/// takes the rest of the width.
#[allow(clippy::too_many_arguments)]
fn text_field(
    ui: &mut Ui,
    cx: &Cx,
    key: &str,
    label: &str,
    icon: Option<&str>,
    placeholder: &'static str,
    font: egui::FontId,
    password: bool,
    trailing: impl FnOnce(&mut Ui, &Cx),
) {
    let mut value = stored(ui, key);
    widgets::field(ui, cx, label, icon, false, |ui| {
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            trailing(ui, cx);
            ui.add(
                TextEdit::singleline(&mut value)
                    .hint_text(placeholder)
                    .font(font)
                    .password(password)
                    .frame(Frame::NONE)
                    .desired_width(ui.available_width()),
            );
        });
    });
    store(ui, key, value);
}

/// One row of the Refused checklist. Its icon is set per row (shield-x on
/// the failed step), which the shared `conn_steps` widget cannot do.
fn refused_row(ui: &mut Ui, cx: &Cx, icon: &str, color: Color32, label: &str, detail: &str, failed: bool) {
    let fill = if failed { theme::SURFACE } else { Color32::TRANSPARENT };
    let frame = Frame::NONE
        .fill(fill)
        .corner_radius(theme::R_FIELD)
        .inner_margin(Margin { left: 12, right: 12, top: 0, bottom: 0 });
    contract::framed(ui, "check-row", frame, |ui| {
            ui.set_width(ui.available_width());
            // A 40 px row; `horizontal_centered` would claim all remaining height.
            ui.allocate_ui_with_layout(vec2(ui.available_width(), 40.0), Layout::left_to_right(Align::Center), |ui| {
                ui.set_min_height(40.0);
                widgets::icon(ui, cx, icon, theme::ICON, color);
                ui.add_space(12.0);
                let text = RichText::new(label).font(if failed { theme::bold(14.0) } else { theme::body(14.0) });
                ui.label(text.color(if failed { theme::INK } else { theme::INK_MUTED }));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    widgets::mono_text(ui, detail, 11.0, theme::INK_FAINT);
                });
            });
        });
}

/// The pulsing bell in the approval pane: a ring grows from 0.92 to 1.12
/// of its size and fades over 2.2 s, as in the mockup. Repainted every
/// 33 ms while shown.
fn pulse(ui: &mut Ui, cx: &Cx) {
    let (rect, _) = ui.allocate_exact_size(vec2(64.0, 64.0), Sense::hover());
    let t = ui.input(|i| i.time);
    let phase = ((t % 2.2) / 2.2) as f32;
    ui.ctx().request_repaint_after(Duration::from_millis(33));
    let p = ui.painter();
    p.rect_filled(rect, theme::R_PANEL, theme::SURFACE);
    // Mockup `.pulse::after`: a rounded-square ring 6 px out (radii
    // 16/22/14/20) that grows and fades; it was a circle before.
    let grow = 6.0 * (0.4 + 0.8 * phase);
    let alpha = 0.7 * (1.0 - phase);
    p.rect_stroke(
        rect.expand(grow),
        egui::CornerRadius { nw: 16, ne: 22, se: 14, sw: 20 },
        Stroke::new(1.5, theme::SIGNAL.linear_multiply(alpha)),
        egui::StrokeKind::Middle,
    );
    cx.icons.paint(p, Rect::from_center_size(rect.center(), vec2(22.0, 22.0)), "bell-ring", theme::SIGNAL);
}

/// Indeterminate progress bar in apricot: a segment slides across a thread
/// track, one sweep every 1.4 s.
fn indeterminate_bar(ui: &mut Ui) {
    ui.add_space(22.0);
    let width = ui.available_width().min(theme::PANE_MAX_W);
    let (rect, _) = ui.allocate_exact_size(vec2(width, 3.0), Sense::hover());
    let t = ui.input(|i| i.time);
    ui.ctx().request_repaint_after(Duration::from_millis(33));
    let p = ui.painter();
    p.rect_filled(rect, 2.0, theme::THREAD);
    let seg_w = rect.width() * 0.30;
    let phase = ((t % 1.4) / 1.4) as f32;
    let x = rect.left() - seg_w + phase * (rect.width() + seg_w);
    let seg = Rect::from_min_max(
        pos2(x.max(rect.left()), rect.top()),
        pos2((x + seg_w).min(rect.right()), rect.bottom()),
    );
    if seg.width() > 0.0 {
        p.rect_filled(seg, 2.0, theme::SIGNAL);
    }
}

// ------------------------------------------------------------ helpers

fn pc_name(state: &AppState, pc_id: &str) -> String {
    state
        .pcs
        .iter()
        .find(|p| p.id == pc_id)
        .map(|p| p.name.clone())
        .unwrap_or_else(|| pc_id.to_string())
}

fn mode_name(mode: StreamMode) -> &'static str {
    match mode {
        StreamMode::Work => "Work",
        StreamMode::Game => "Game",
    }
}

/// `m:ss` for the waiting timers.
fn clock(secs: u32) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// The 6-digit comparison number in the "482 913" form.
fn number_text(n: u32) -> String {
    format!("{:03} {:03}", n / 1000, n % 1000)
}

fn short(id: &str, n: usize) -> String {
    id.chars().take(n).collect()
}

/// Crockford base32 without the letters it leaves out (I, L, O, U).
const CROCKFORD: &str = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// A typed PC ID in the shown form: upper case, groups of four.
fn normalize_id(raw: &str) -> String {
    let chars: Vec<char> = raw.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_uppercase()).collect();
    chars.chunks(4).map(|c| c.iter().collect::<String>()).collect::<Vec<_>>().join("-")
}

/// 16 Crockford base32 characters, as `normalize_id` returns them.
fn valid_id(id: &str) -> bool {
    let chars: Vec<char> = id.chars().filter(|c| *c != '-').collect();
    chars.len() == 16 && chars.iter().all(|c| CROCKFORD.contains(*c))
}

fn stored(ui: &mut Ui, key: &str) -> String {
    ui.data_mut(|d| d.get_temp_mut_or_default::<String>(Id::new(("flow-input", key))).clone())
}

fn store(ui: &mut Ui, key: &str, value: String) {
    ui.data_mut(|d| *d.get_temp_mut_or_default::<String>(Id::new(("flow-input", key))) = value);
}
