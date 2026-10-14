//! Screens, widgets, theme, fonts and icons.
//!
//! [`App`] implements the shell's [`ShellApp`]: each frame it takes one
//! snapshot from the [`Backend`], picks the screen, and sends the actions
//! the screens collected back to the backend.
//!
//! Which screen shows:
//! - `state.session` is Some: the session (video plus strip overlays).
//! - else `state.flow` is Some: the flow screen for it (step rail + pane).
//! - else `UiState::view`: Home (a PC or This PC selected) or Settings.

use std::time::Instant;

use crate::model::{Action, AppState, Backend};
use crate::shell::{FrameRequests, ShellApp, WindowCommand, WindowInfo};

pub mod theme;
pub mod fonts;
pub mod icons;
pub mod widgets;
pub mod home;
pub mod flow;
pub mod settings;
pub mod session;

pub use icons::Icons;

/// UI-only state: what the backend does not know about.
#[derive(Clone, Debug, PartialEq)]
pub struct UiState {
    pub view: View,
    pub session: SessionUi,
    /// Shown small in the title bar in demo mode.
    pub demo_label: Option<String>,
}

impl Default for UiState {
    fn default() -> Self {
        Self { view: View::Home(HomeSel::FirstPc), session: SessionUi::default(), demo_label: None }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum View {
    Home(HomeSel),
    Settings,
}

#[derive(Clone, Debug, PartialEq)]
pub enum HomeSel {
    /// The first remote PC (or This PC if there are none).
    FirstPc,
    Pc(String),
    ThisPc,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionUi {
    pub strip: StripView,
    pub stats_open: bool,
    pub toast: Option<Toast>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StripView {
    /// Only the small tab on the top edge.
    #[default]
    Collapsed,
    Toolbar,
    MonitorPicker,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Toast {
    /// Capture started: shows the release chord.
    Captured,
    /// After Ctrl+Alt+Shift+Z: input released.
    Released,
}

/// Drives `--ui-demo`: each call moves the backend to the next scene and
/// returns the UI state that goes with it.
pub trait DemoDriver {
    /// Number of scenes in one cycle.
    fn len(&self) -> usize;
    /// Apply scene `index` (0-based, < len) and return its UI state; the
    /// label names the scene (used for the title bar and screenshot names).
    fn scene(&mut self, index: usize) -> (String, UiState);
}

/// Per-frame context the screens draw with.
pub struct Cx<'a> {
    pub icons: &'a Icons,
    /// Actions for the backend, sent after the frame.
    pub actions: &'a mut Vec<Action>,
    /// The shell's per-frame requests (drag regions, video rect).
    pub out: &'a mut FrameRequests,
    pub window: WindowInfo,
}

pub struct App {
    backend: Box<dyn Backend>,
    ui: UiState,
    icons: Option<Icons>,
    demo: Option<Demo>,
}

struct Demo {
    driver: Box<dyn DemoDriver>,
    index: usize,
    /// `--shots <dir>`: capture each scene once, then close.
    shots: Option<std::path::PathBuf>,
    /// Frames rendered since the scene changed (captures wait for layout
    /// and textures to settle).
    settle: u32,
    started: Instant,
}

impl App {
    pub fn new(backend: Box<dyn Backend>) -> Self {
        Self { backend, ui: UiState::default(), icons: None, demo: None }
    }

    /// Demo mode: F12 cycles scenes. With `shots`, every scene is captured
    /// to `<dir>/NN-label.png` and the window closes after the last one.
    pub fn with_demo(mut self, mut driver: Box<dyn DemoDriver>, shots: Option<std::path::PathBuf>) -> Self {
        let (label, ui) = driver.scene(0);
        self.ui = UiState { demo_label: Some(label), ..ui };
        self.demo = Some(Demo { driver, index: 0, shots, settle: 0, started: Instant::now() });
        self
    }

    fn demo_step(&mut self, ctx: &egui::Context, out: &mut FrameRequests) {
        let Some(demo) = self.demo.as_mut() else { return };
        let next = ctx.input(|i| i.key_pressed(egui::Key::F12));
        if let Some(dir) = demo.shots.clone() {
            // Give fonts, icon textures, layout and fade-ins time to settle:
            // at least 4 frames and 0.8 s per scene.
            ctx.request_repaint();
            let ready = demo.started.elapsed() >= std::time::Duration::from_millis(800);
            if demo.settle < 4 || (demo.settle == 4 && !ready) {
                demo.settle = (demo.settle + 1).min(4);
                return;
            }
            demo.settle += 1;
            if demo.settle == 5 {
                let label = self.ui.demo_label.clone().unwrap_or_default();
                out.capture = Some(dir.join(format!("{:02}-{}.png", demo.index + 1, slug(&label))));
            } else if demo.settle > 5 {
                if demo.index + 1 >= demo.driver.len() {
                    out.command = Some(WindowCommand::Close);
                    return;
                }
                demo.index += 1;
                demo.settle = 0;
                demo.started = Instant::now();
                let (label, ui) = demo.driver.scene(demo.index);
                self.ui = UiState { demo_label: Some(label), ..ui };
            }
            return;
        }
        if next {
            demo.index = (demo.index + 1) % demo.driver.len();
            let (label, ui) = demo.driver.scene(demo.index);
            self.ui = UiState { demo_label: Some(label), ..ui };
        }
    }
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

impl ShellApp for App {
    fn frame(&mut self, root: &mut egui::Ui, info: WindowInfo, out: &mut FrameRequests) {
        let ctx_owned = root.ctx().clone();
        let ctx = &ctx_owned;
        if self.icons.is_none() {
            fonts::install(ctx);
            widgets::install_style(ctx);
            self.icons = Some(Icons::load(ctx));
            // egui applies set_fonts on the next pass; draw nothing until then.
            ctx.request_repaint();
            return;
        }
        self.demo_step(ctx, out);
        let state: AppState = self.backend.snapshot();
        let mut actions = Vec::new();
        {
            let icons = self.icons.as_ref().expect("icons loaded");
            let mut cx = Cx { icons, actions: &mut actions, out, window: info };
            if let Some(sess) = state.session.as_ref() {
                session::show(ctx, &mut cx, &state, sess, &mut self.ui);
            } else {
                let ctx_label = titlebar_context(&state, &self.ui);
                widgets::titlebar(root, &mut cx, &ctx_label, self.ui.demo_label.as_deref());
                egui::CentralPanel::default()
                    .frame(egui::Frame::NONE.fill(theme::FIELD))
                    .show(root, |ui| {
                        if let Some(flow) = state.flow.as_ref() {
                            flow::show(ui, &mut cx, &state, flow, &mut self.ui);
                        } else {
                            match self.ui.view.clone() {
                                View::Home(_) => home::show(ui, &mut cx, &state, &mut self.ui),
                                View::Settings => settings::show(ui, &mut cx, &state, &mut self.ui),
                            }
                        }
                    });
            }
        }
        for a in actions {
            self.backend.act(a);
        }
    }
}

/// The context text next to "BubblePlay" in the title bar.
fn titlebar_context(state: &AppState, ui: &UiState) -> String {
    use crate::model::Flow;
    let pc_name = |id: &str| {
        state.pcs.iter().find(|p| p.id == id).map(|p| p.name.clone()).unwrap_or_default()
    };
    match &state.flow {
        Some(Flow::PairEnter { .. }) | Some(Flow::PairConfirm { .. }) => "Pair a new PC".into(),
        Some(Flow::Password { pc_name, .. }) => pc_name.clone(),
        Some(Flow::Approval { pc_id, .. })
        | Some(Flow::Connecting { pc_id, .. })
        | Some(Flow::Failed { pc_id, .. })
        | Some(Flow::Refused { pc_id }) => pc_name(pc_id),
        Some(Flow::Rotate { .. }) => "This device".into(),
        None => match &ui.view {
            View::Settings => "This device".into(),
            View::Home(HomeSel::ThisPc) => "This PC, hosting".into(),
            View::Home(_) => format!("{} PCs", state.pcs.len()),
        },
    }
}
