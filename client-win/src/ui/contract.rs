//! Layout contract: marks in egui's accessibility tree that the layout tests
//! (client-win/tests/ui_layout.rs) check.
//!
//! The tree already holds every label, button and focusable widget with its
//! bounds. What it does not know is which rect is a card or a row (egui's
//! `Ui` nodes carry no bounds), where a line of text sits on its baseline,
//! and whether a text may be cut short. The helpers here add that:
//!
//! - Containers: a `Ui` node gets `author_id` `box:<name>`, `row:<name>` or
//!   `scroll:<name>` and the container's rect as bounds. Everything drawn in
//!   that `Ui` must stay inside it; siblings in it must not overlap.
//! - Texts: a label node gets `class_name` `text b=<baseline>` plus ` trunc`
//!   when it may be cut short and ` elided` when it was.
//! - Painted pieces (pills, chords, painted text) get a node of their own so
//!   they take part in the checks like any widget.
//!
//! Outside tests nothing reads these marks; they cost one map write per
//! item and also give screen readers names for painted pieces.

use std::sync::Arc;

use egui::{
    accesskit, Align2, Color32, FontId, Frame, Galley, Id, InnerResponse, Layout, Pos2, Rect,
    Response, Sense, Ui, UiBuilder, WidgetInfo, WidgetText, WidgetType,
};

pub const BOX: &str = "box:";
pub const ROW: &str = "row:";
pub const SCROLL: &str = "scroll:";

/// Marks the node of `ui_id` as a container of `kind` (one of the prefixes
/// above) named `name`, with `rect` as its bounds.
pub fn mark(ctx: &egui::Context, ui_id: Id, kind: &str, name: &str, rect: Rect) {
    ctx.accesskit_node_builder(ui_id, |node| {
        node.set_role(accesskit::Role::Group);
        node.set_author_id(format!("{kind}{name}"));
        node.set_bounds(ak_rect(rect));
    });
}

/// A child `Ui` laid out inside `rect`, marked as container `name`. Does not
/// move the parent's cursor.
pub fn boxed<R>(
    ui: &mut Ui,
    name: &str,
    rect: Rect,
    layout: Layout,
    add: impl FnOnce(&mut Ui) -> R,
) -> R {
    let mut child = ui.new_child(UiBuilder::new().max_rect(rect).layout(layout));
    let id = child.unique_id();
    let r = add(&mut child);
    mark(ui.ctx(), id, BOX, name, rect);
    r
}

/// A row of fixed height across the available width, children centred
/// vertically, marked as row `name`. Texts in it must share a baseline.
pub fn row<R>(
    ui: &mut Ui,
    name: &str,
    height: f32,
    add: impl FnOnce(&mut Ui) -> R,
) -> InnerResponse<R> {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), height), Sense::hover());
    let mut child = ui.new_child(
        UiBuilder::new()
            .max_rect(rect)
            .layout(Layout::left_to_right(egui::Align::Center)),
    );
    let id = child.unique_id();
    let r = add(&mut child);
    mark(ui.ctx(), id, ROW, name, rect);
    InnerResponse::new(r, ui.interact(rect, id.with("row"), Sense::hover()))
}

/// A row inside `rect` (not allocated in the parent), children centred
/// vertically. Use when the row's rect is computed, e.g. inside a card.
pub fn row_at<R>(ui: &mut Ui, name: &str, rect: Rect, add: impl FnOnce(&mut Ui) -> R) -> R {
    let mut child = ui.new_child(
        UiBuilder::new()
            .max_rect(rect)
            .layout(Layout::left_to_right(egui::Align::Center)),
    );
    let id = child.unique_id();
    let r = add(&mut child);
    mark(ui.ctx(), id, ROW, name, rect);
    r
}

/// `frame.show` with the frame's outer rect marked as container `name`.
pub fn framed<R>(
    ui: &mut Ui,
    name: &str,
    frame: Frame,
    add: impl FnOnce(&mut Ui) -> R,
) -> InnerResponse<R> {
    let mut prepared = frame.begin(ui);
    let id = prepared.content_ui.unique_id();
    let r = add(&mut prepared.content_ui);
    let resp = prepared.end(ui);
    mark(ui.ctx(), id, BOX, name, resp.rect);
    InnerResponse::new(r, resp)
}

/// A vertical scroll area whose viewport is marked as `scroll:<name>`.
pub fn scroll<R>(
    ui: &mut Ui,
    name: &str,
    area: egui::ScrollArea,
    add: impl FnOnce(&mut Ui) -> R,
) -> egui::scroll_area::ScrollAreaOutput<R> {
    let mut content_id = None;
    // Salted with the contract name: sibling scroll areas (the sidebar list
    // and the This PC body) otherwise share an id, which egui flags on screen
    // in debug builds and which mixes their scroll offsets.
    let out = area.id_salt(("contract-scroll", name)).show(ui, |ui| {
        content_id = Some(ui.unique_id());
        add(ui)
    });
    if let Some(id) = content_id {
        mark(ui.ctx(), id, SCROLL, name, out.inner_rect);
    }
    out
}

// ------------------------------------------------------------ texts

/// One line of text. It never wraps; with `may_truncate` it is cut short
/// with an ellipsis to fit the space left, otherwise it keeps its full width
/// (and the layout tests fail if that leaves its container).
pub fn text(ui: &mut Ui, text: impl Into<WidgetText>, may_truncate: bool) -> Response {
    let label = egui::Label::new(text).selectable(false);
    let label = if may_truncate {
        label.truncate()
    } else {
        label.extend()
    };
    let (pos, galley, resp) = label.layout_in_ui(ui);
    resp.widget_info(|| WidgetInfo::labeled(WidgetType::Label, ui.is_enabled(), galley.text()));
    if ui.is_rect_visible(resp.rect) {
        let fallback = ui.style().visuals.text_color();
        ui.painter().galley(pos, galley.clone(), fallback);
    }
    if galley.elided && may_truncate {
        // The full text on hover, as egui's own truncated labels do.
        let full = galley.text().to_owned();
        mark_text(ui, resp.id, pos, &galley, may_truncate);
        return resp.on_hover_text(full);
    }
    mark_text(ui, resp.id, pos, &galley, may_truncate);
    resp
}

/// Painted text (no wrap) at `pos` with `anchor`, registered as a label node
/// so the layout tests see it. Returns its rect.
#[allow(clippy::too_many_arguments)]
pub fn paint_text(
    ui: &mut Ui,
    pos: Pos2,
    anchor: Align2,
    text: &str,
    font: FontId,
    color: Color32,
) -> Rect {
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, color);
    let rect = anchor.anchor_size(pos, galley.size());
    ui.painter().galley(rect.min, galley.clone(), color);
    let id = ui
        .id()
        .with(("ptext", text, rect.min.x as i32, rect.min.y as i32));
    let resp = ui.interact(rect, id, Sense::hover());
    resp.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, text));
    mark_text(ui, id, rect.min, &galley, false);
    rect
}

/// Records baseline and truncation of a laid-out galley on node `id`.
pub fn mark_text(ui: &Ui, id: Id, pos: Pos2, galley: &Arc<Galley>, may_truncate: bool) {
    let baseline = galley
        .rows
        .first()
        .and_then(|row| row.glyphs.first().map(|g| pos.y + row.pos.y + g.pos.y));
    let mut class = match baseline {
        Some(b) => format!("text b={b:.2}"),
        None => "text".to_owned(),
    };
    if galley.rows.len() > 1 {
        class.push_str(" multi");
    }
    if may_truncate {
        class.push_str(" trunc");
    }
    if galley.elided {
        class.push_str(" elided");
    }
    ui.ctx()
        .accesskit_node_builder(id, |node| node.set_class_name(class));
}

/// Gives a painted widget's response a node: `kind` goes into `class_name`
/// (pill, chord, switch) and `label` is what a screen reader reads.
pub fn mark_widget(resp: &Response, typ: WidgetType, kind: &str, label: &str) {
    let enabled = resp.enabled();
    resp.widget_info(|| WidgetInfo::labeled(typ, enabled, label));
    let kind = kind.to_owned();
    resp.ctx
        .accesskit_node_builder(resp.id, |node| node.set_class_name(kind));
}

fn ak_rect(r: Rect) -> accesskit::Rect {
    accesskit::Rect {
        x0: r.min.x.into(),
        y0: r.min.y.into(),
        x1: r.max.x.into(),
        y1: r.max.y.into(),
    }
}
