//! Adapts the existing UI and shell to a bounded development session.
//! All drawing remains in the UI track; this module only supplies state and
//! forwards the shell's existing close/capture requests.

use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use crate::{
    model::{Action, AppState, Backend, Notifier},
    shell::{FrameRequests, ShellApp, WindowCommand, WindowInfo},
    ui,
};

pub struct SessionBackend {
    pub poll: Arc<dyn Fn() -> AppState + Send + Sync>,
    pub stop: Arc<AtomicBool>,
    pub notifier: Option<Notifier>,
}

impl Backend for SessionBackend {
    fn snapshot(&self) -> AppState {
        (self.poll)()
    }
    fn act(&mut self, action: Action) {
        if action == Action::Disconnect {
            self.stop.store(true, Ordering::Release);
            if let Some(notify) = &self.notifier {
                notify();
            }
        }
    }
    fn set_notifier(&mut self, notify: Notifier) {
        self.notifier = Some(notify);
    }
}

pub struct DevApp {
    pub ui: ui::App,
    pub pacing: super::pacing::Pacing,
    pub stop: Arc<AtomicBool>,
    pub started: Instant,
    pub exit_after: Option<Duration>,
    pub capture: Option<PathBuf>,
    pub has_video: Arc<dyn Fn() -> bool + Send + Sync>,
    /// One-shot measurement boundary while the last shell frame is still live.
    pub before_close: Option<Box<dyn FnOnce()>>,
}

impl ShellApp for DevApp {
    fn frame(&mut self, ui: &mut egui::Ui, info: WindowInfo, out: &mut FrameRequests) {
        self.pacing.begin_frame();
        self.ui.frame(ui, info, out);
        if self.started.elapsed() >= Duration::from_secs(5) && (self.has_video)() {
            if let Some(path) = self.capture.take() {
                out.capture = Some(path);
            }
        }
        if matches!(out.command, Some(WindowCommand::Close))
            || self.stop.load(Ordering::Acquire)
            || self
                .exit_after
                .is_some_and(|after| self.started.elapsed() >= after)
        {
            if let Some(before_close) = self.before_close.take() {
                before_close();
            }
            out.command = Some(WindowCommand::Close);
        }
        self.pacing.mark("ui_frame_return");
    }
}
