//! Layout tests for the native viewer UI: every `--ui-demo` scene, at the
//! default window size and at the shell's minimum size, checked on egui's
//! accessibility tree, plus snapshot PNGs at the default size.
//!
//! The UI modules are compiled into this test crate directly (the client is
//! a binary crate); the marks they leave in the tree are described in
//! src/ui/contract.rs.
//!
//! Invariants:
//! - every text and widget lies inside its container (card, row, panel);
//! - siblings in one container do not overlap;
//! - no text is elided unless it is marked as allowed to truncate;
//! - nothing lies outside the window;
//! - buttons are at least 32 px high;
//! - texts in one row share a baseline;
//! - no more than three texts run together on one line (labelled fields or
//!   a second line instead).
//!
//! Snapshots: every scene at the default size, and the login, pairing,
//! approval and saved-PC scenes (v1 step 4) at the minimum size too, as
//! `NN-name-min.png`.
//!
//! Update the snapshots with `UPDATE_SNAPSHOTS=1 cargo test --test ui_layout`.

#![allow(dead_code)]

#[path = "../src/model.rs"]
mod model;
#[path = "../src/mock.rs"]
mod mock;
#[path = "../src/shell/mod.rs"]
mod shell;
#[path = "../src/ui/mod.rs"]
mod ui;

use egui::{pos2, vec2, Rect};
use egui_kittest::kittest::{AccessKitNode, NodeT};
use egui_kittest::{Harness, SnapshotOptions};

/// The window size `--ui-demo` opens at.
const FULL: (f32, f32) = (ui::theme::WINDOW_W, ui::theme::WINDOW_H);
/// Rounding slack for rect comparisons, in points.
const TOL: f32 = 0.5;
/// Largest baseline difference allowed between texts in one row.
const BASELINE_TOL: f32 = 1.0;
const MIN_BUTTON_H: f32 = 32.0;
/// Frames run before checking: fonts and icons load on the first frame,
/// layout settles on the following ones.
const SETTLE_STEPS: usize = 6;

struct St {
    app: ui::App,
    out: shell::FrameRequests,
}

/// The shell's minimum window size, read from `ShellConfig { min_size }` in
/// main.rs so the test follows it.
fn min_size() -> (f32, f32) {
    let src = include_str!("../src/main.rs");
    let at = src.find("min_size:").expect("main.rs sets ShellConfig::min_size");
    let rest = &src[at..];
    let open = rest.find('(').expect("min_size tuple");
    let close = rest.find(')').expect("min_size tuple");
    let nums: Vec<f32> = rest[open + 1..close]
        .split(',')
        .map(|n| n.trim().parse::<f32>().expect("min_size is two numbers"))
        .collect();
    (nums[0], nums[1])
}

fn harness(scene: usize, size: (f32, f32)) -> (Harness<'static, St>, String) {
    let (backend, driver) = mock::new();
    let mut app = ui::App::new(Box::new(backend)).with_demo(Box::new(driver), None);
    let label = app.demo_scene(scene).expect("demo mode");
    let st = St { app, out: shell::FrameRequests::default() };
    let mut h = Harness::builder()
        .with_size(vec2(size.0, size.1))
        .with_pixels_per_point(1.0)
        .build_ui_state(
            |kit_ui, st: &mut St| {
                // kittest wraps the closure in a margin frame; the shell hands
                // the app a root Ui spanning the whole window, so make one.
                let ctx = kit_ui.ctx().clone();
                let mut root = egui::Ui::new(
                    ctx.clone(),
                    egui::Id::new("test-root"),
                    egui::UiBuilder::new()
                        .layer_id(egui::LayerId::background())
                        .max_rect(ctx.content_rect()),
                );
                st.out = shell::FrameRequests::default();
                let info = shell::WindowInfo { maximized: false };
                <ui::App as shell::ShellApp>::frame(&mut st.app, &mut root, info, &mut st.out);
            },
            st,
        );
    h.run_steps(SETTLE_STEPS);
    (h, format!("{:02}-{}", scene + 1, slug(&label)))
}

fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

// ------------------------------------------------------------ tree walk

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Box,
    Row,
    Scroll,
}

struct Container {
    name: String,
    kind: Kind,
    rect: Rect,
    /// Index of the enclosing container.
    parent: Option<usize>,
}

struct Item {
    desc: String,
    rect: Rect,
    container: Option<usize>,
    /// This item is itself container `index` (checked as a member of its parent).
    is_container: Option<usize>,
    button: bool,
    class: String,
}

struct Tree {
    containers: Vec<Container>,
    items: Vec<Item>,
}

fn rect_of(node: &AccessKitNode<'_>) -> Option<Rect> {
    let b = node.bounding_box()?;
    let r = Rect::from_min_max(pos2(b.x0 as f32, b.y0 as f32), pos2(b.x1 as f32, b.y1 as f32));
    (r.is_finite() && r.is_positive()).then_some(r)
}

fn describe(node: &AccessKitNode<'_>) -> String {
    let text = node.data().label().map(str::to_owned).or_else(|| node.value()).unwrap_or_default();
    let class = node.class_name().map(|c| format!(" [{}]", c.split(' ').next().unwrap_or(""))).unwrap_or_default();
    format!("{:?}{class} '{}'", node.role(), text.chars().take(48).collect::<String>())
}

fn walk(node: AccessKitNode<'_>, container: Option<usize>, tree: &mut Tree) {
    let mut current = container;
    if let Some(author) = node.author_id() {
        let kind = if let Some(n) = author.strip_prefix(ui::contract::BOX) {
            Some((Kind::Box, n))
        } else if let Some(n) = author.strip_prefix(ui::contract::ROW) {
            Some((Kind::Row, n))
        } else {
            author.strip_prefix(ui::contract::SCROLL).map(|n| (Kind::Scroll, n))
        };
        if let (Some((kind, name)), Some(rect)) = (kind, rect_of(&node)) {
            let index = tree.containers.len();
            tree.containers.push(Container { name: name.to_owned(), kind, rect, parent: container });
            tree.items.push(Item {
                desc: author.to_string(),
                rect,
                container,
                is_container: Some(index),
                button: false,
                class: String::new(),
            });
            current = Some(index);
        }
    } else if !matches!(node.role(), egui::accesskit::Role::GenericContainer | egui::accesskit::Role::Window) {
        if let Some(rect) = rect_of(&node) {
            tree.items.push(Item {
                desc: describe(&node),
                rect,
                container,
                is_container: None,
                button: node.role() == egui::accesskit::Role::Button,
                class: node.class_name().unwrap_or("").to_owned(),
            });
            // A widget's own children (text runs of a text box) belong to it.
            return;
        }
    }
    for child in node.children() {
        walk(child, current, tree);
    }
}

fn contains(outer: Rect, inner: Rect) -> bool {
    inner.min.x >= outer.min.x - TOL
        && inner.min.y >= outer.min.y - TOL
        && inner.max.x <= outer.max.x + TOL
        && inner.max.y <= outer.max.y + TOL
}

fn same(a: Rect, b: Rect) -> bool {
    (a.min - b.min).length() <= TOL && (a.max - b.max).length() <= TOL
}

fn overlaps(a: Rect, b: Rect) -> bool {
    let i = a.intersect(b);
    i.width() > TOL && i.height() > TOL
}

fn fmt_rect(r: Rect) -> String {
    format!("[{:.1},{:.1} {:.1}x{:.1}]", r.min.x, r.min.y, r.width(), r.height())
}

/// Most texts allowed in one run on a line, and the gap that ends a run.
const MAX_RUN: usize = 3;
const RUN_GAP: f32 = 16.0;

/// A label or pill (not a tag, key chord or switch).
fn is_text(it: &Item) -> bool {
    it.is_container.is_none()
        && it.desc.starts_with("Label")
        && !["[tag]", "[chord]", "[switch]"].iter().any(|c| it.desc.contains(c))
}

fn baseline(class: &str) -> Option<f32> {
    if class.contains(" multi") {
        return None;
    }
    class.split(' ').find_map(|p| p.strip_prefix("b=")).and_then(|b| b.parse().ok())
}

/// Every broken invariant in the harness's current frame, one line each.
fn check(h: &Harness<'_, St>, size: (f32, f32)) -> Vec<String> {
    let mut tree = Tree { containers: Vec::new(), items: Vec::new() };
    walk(h.root().accesskit_node(), None, &mut tree);
    let window = Rect::from_min_size(pos2(0.0, 0.0), vec2(size.0, size.1));
    let mut out = Vec::new();

    // egui paints "First use of ... ID" / "Second use of ..." over the UI in
    // debug builds when two widgets share an id; it is not in the accesskit
    // tree, so the painted text shapes are searched for it.
    fn painted_texts(shape: &egui::Shape, texts: &mut Vec<String>) {
        match shape {
            egui::Shape::Text(t) => texts.push(t.galley.text().to_owned()),
            egui::Shape::Vec(v) => v.iter().for_each(|s| painted_texts(s, texts)),
            _ => {}
        }
    }
    let mut texts = Vec::new();
    for clipped in &h.output().shapes {
        painted_texts(&clipped.shape, &mut texts);
    }
    for t in texts {
        if t.contains("use of") && t.contains(" ID ") {
            out.push(format!("id clash warning painted: '{t}'"));
        }
    }
    let cname = |i: usize| {
        let c = &tree.containers[i];
        let k = match c.kind {
            Kind::Box => "box",
            Kind::Row => "row",
            Kind::Scroll => "scroll",
        };
        format!("{k}:{}", c.name)
    };

    // Items scrolled out of their viewport are not on screen.
    let visible = |it: &Item| -> bool {
        let mut c = it.container;
        while let Some(i) = c {
            let cont = &tree.containers[i];
            if cont.kind == Kind::Scroll && !overlaps(cont.rect, it.rect) {
                return false;
            }
            c = cont.parent;
        }
        true
    };

    // The part of an item its scroll viewports let through.
    let shown = |it: &Item| -> Rect {
        let mut r = it.rect;
        let mut c = it.container;
        while let Some(i) = c {
            let cont = &tree.containers[i];
            if cont.kind == Kind::Scroll {
                r = r.intersect(cont.rect);
            }
            c = cont.parent;
        }
        r
    };

    for it in &tree.items {
        if !visible(it) {
            continue;
        }
        if !contains(window, shown(it)) {
            out.push(format!("outside the window: {} at {}", it.desc, fmt_rect(it.rect)));
        }
        if let Some(ci) = it.container {
            let c = &tree.containers[ci];
            let inside = match c.kind {
                Kind::Scroll => it.rect.min.x >= c.rect.min.x - TOL && it.rect.max.x <= c.rect.max.x + TOL,
                Kind::Box | Kind::Row => contains(c.rect, it.rect),
            };
            if !inside {
                out.push(format!(
                    "outside its container {} {}: {} at {}",
                    cname(ci),
                    fmt_rect(c.rect),
                    it.desc,
                    fmt_rect(it.rect)
                ));
            }
        }
        if it.button && it.rect.height() < MIN_BUTTON_H - 0.01 {
            out.push(format!("button lower than {MIN_BUTTON_H} px: {} at {}", it.desc, fmt_rect(it.rect)));
        }
        if it.class.contains(" elided") && !it.class.contains(" trunc") {
            out.push(format!("elided text not marked as allowed to truncate: {}", it.desc));
        }
    }

    // Sibling overlap, per container. A member covering the whole container
    // is the container's hit area, not a sibling.
    for ci in 0..tree.containers.len() {
        let crect = tree.containers[ci].rect;
        let members: Vec<&Item> = tree
            .items
            .iter()
            // Scroll bars float over their content by design.
            .filter(|it| {
                it.container == Some(ci) && visible(it) && !same(it.rect, crect) && !it.desc.starts_with("ScrollBar")
            })
            .collect();
        for (a_i, a) in members.iter().enumerate() {
            for b in &members[a_i + 1..] {
                if overlaps(a.rect, b.rect) {
                    out.push(format!(
                        "overlap in {}: {} at {} and {} at {}",
                        cname(ci),
                        a.desc,
                        fmt_rect(a.rect),
                        b.desc,
                        fmt_rect(b.rect)
                    ));
                }
            }
        }
        // Run-on lines: more than MAX_RUN texts (labels and pills) on one
        // line with less than RUN_GAP between neighbours read as one run of
        // words (the old This PC header: status, viewers, "PC ID" and the
        // ID). Such lines need labelled fields or a second line.
        let mut texts: Vec<&Item> = members.iter().copied().filter(|it| is_text(it)).collect();
        texts.sort_by(|a, b| a.rect.center().y.total_cmp(&b.rect.center().y).then(a.rect.min.x.total_cmp(&b.rect.min.x)));
        let mut used = vec![false; texts.len()];
        for i in 0..texts.len() {
            if used[i] {
                continue;
            }
            let cy = texts[i].rect.center().y;
            let mut line: Vec<usize> =
                (i..texts.len()).filter(|&j| !used[j] && (texts[j].rect.center().y - cy).abs() <= 3.0).collect();
            line.sort_by(|&a, &b| texts[a].rect.min.x.total_cmp(&texts[b].rect.min.x));
            for &j in &line {
                used[j] = true;
            }
            let mut run = vec![line[0]];
            let flush = |run: &Vec<usize>, out: &mut Vec<String>| {
                if run.len() > MAX_RUN {
                    let words: Vec<String> = run.iter().map(|&k| texts[k].desc.clone()).collect();
                    out.push(format!("run-on line in {} ({} texts): {}", cname(ci), run.len(), words.join(", ")));
                }
            };
            for w in line.windows(2) {
                let gap = texts[w[1]].rect.min.x - texts[w[0]].rect.max.x;
                if gap < RUN_GAP {
                    run.push(w[1]);
                } else {
                    flush(&run, &mut out);
                    run = vec![w[1]];
                }
            }
            flush(&run, &mut out);
        }
        if tree.containers[ci].kind == Kind::Row {
            let lines: Vec<(f32, &Item)> = members.iter().filter_map(|it| baseline(&it.class).map(|b| (b, *it))).collect();
            if let (Some(lo), Some(hi)) = (
                lines.iter().min_by(|a, b| a.0.total_cmp(&b.0)),
                lines.iter().max_by(|a, b| a.0.total_cmp(&b.0)),
            ) {
                if hi.0 - lo.0 > BASELINE_TOL {
                    out.push(format!(
                        "baselines differ by {:.1} px in {}: {} at y {:.1}, {} at y {:.1}",
                        hi.0 - lo.0,
                        cname(ci),
                        lo.1.desc,
                        lo.0,
                        hi.1.desc,
                        hi.0
                    ));
                }
            }
        }
    }
    out
}

fn scene_count() -> usize {
    let (backend, driver) = mock::new();
    ui::App::new(Box::new(backend)).with_demo(Box::new(driver), None).demo_len()
}

fn layout_at(size: (f32, f32)) {
    let mut report = Vec::new();
    let mut broken = 0;
    for scene in 0..scene_count() {
        let (h, name) = harness(scene, size);
        let problems = check(&h, size);
        if !problems.is_empty() {
            broken += 1;
        }
        for p in problems {
            report.push(format!("[{name} @{}x{}] {p}", size.0, size.1));
        }
    }
    for line in &report {
        println!("{line}");
    }
    assert!(report.is_empty(), "{} layout problems in {broken} scenes at {}x{} (listed above)", report.len(), size.0, size.1);
}

#[test]
fn layout_at_default_size() {
    layout_at(FULL);
}

#[test]
fn layout_at_minimum_size() {
    layout_at(min_size());
}

#[test]
fn snapshots_at_default_size() {
    // Font hinting and the GPU's rasteriser vary a little between machines;
    // a few hundred changed pixels pass, real layout changes do not.
    let options = SnapshotOptions::new().threshold(0.8).failed_pixel_count_threshold(400);
    let mut results = Vec::new();
    for scene in 0..scene_count() {
        let (mut h, name) = harness(scene, FULL);
        results.push(h.try_snapshot_options(name, &options));
    }
    let failed: Vec<String> = results.into_iter().filter_map(|r| r.err().map(|e| e.to_string())).collect();
    for f in &failed {
        println!("{f}");
    }
    assert!(failed.is_empty(), "{} snapshots differ (listed above)", failed.len());
}

/// Scenes snapshotted at the minimum size as well: the step 4 screens (from
/// 23 on) and the earlier pairing scenes step 4 changed (enter, confirm,
/// approved).
fn min_size_snapshot_scenes() -> Vec<usize> {
    let mut scenes = vec![5, 6, 7];
    scenes.extend(23..scene_count());
    scenes
}

#[test]
fn snapshots_at_minimum_size() {
    let options = SnapshotOptions::new().threshold(0.8).failed_pixel_count_threshold(400);
    let mut results = Vec::new();
    for scene in min_size_snapshot_scenes() {
        let (mut h, name) = harness(scene, min_size());
        results.push(h.try_snapshot_options(format!("{name}-min"), &options));
    }
    let failed: Vec<String> = results.into_iter().filter_map(|r| r.err().map(|e| e.to_string())).collect();
    for f in &failed {
        println!("{f}");
    }
    assert!(failed.is_empty(), "{} snapshots differ (listed above)", failed.len());
}
