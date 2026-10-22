//! Capture policy independent of Win32 and transport I/O.
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Work,
    Game,
}
impl Mode {
    pub fn stream_mode(self) -> &'static str {
        match self {
            Self::Work => "work",
            Self::Game => "game",
        }
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct CaptureSnapshot {
    pub mode: Mode,
    pub captured: bool,
    pub released: bool,
    pub focused: bool,
    pub video_focused: bool,
    pub input_ready: bool,
    pub hook_installed: bool,
    pub clipped: bool,
    pub epoch: u64,
    pub reason: String,
    pub error: Option<String>,
}
#[derive(Clone)]
pub struct CaptureStatus(Arc<Mutex<CaptureSnapshot>>);
impl CaptureStatus {
    pub fn snapshot(&self) -> CaptureSnapshot {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    pub(super) fn publish(&self, value: CaptureSnapshot) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = value;
    }
}
pub struct CaptureState {
    pub mode: Mode,
    pub requested: bool,
    pub focused: bool,
    pub video_focused: bool,
    pub ready: bool,
    pub rect: bool,
    pub active: bool,
    pub epoch: u64,
    pub reason: &'static str,
}
impl CaptureState {
    pub fn new(mode: Mode) -> Self {
        Self {
            mode,
            requested: true,
            focused: false,
            video_focused: false,
            ready: false,
            rect: false,
            active: true,
            epoch: 0,
            reason: "starting",
        }
    }
    pub fn eligible(&self) -> bool {
        self.active && self.requested && self.focused && self.ready && self.rect
    }
    /// Work pointer input needs only the video hit-test. Keyboard/egui capture
    /// engages when the user gives the video focus. Game captures both.
    pub fn captured(&self) -> bool {
        self.eligible() && (self.mode == Mode::Game || self.video_focused)
    }
    pub fn toggle(&mut self) {
        self.requested = !self.requested;
        self.reason = if self.requested {
            "recapture-chord"
        } else {
            "release-chord"
        };
    }
    pub fn snapshot(&self, hook: bool, clipped: bool, error: Option<String>) -> CaptureSnapshot {
        CaptureSnapshot {
            mode: self.mode,
            captured: self.captured(),
            released: !self.requested,
            focused: self.focused,
            video_focused: self.video_focused,
            input_ready: self.ready,
            hook_installed: hook,
            clipped,
            epoch: self.epoch,
            reason: self.reason.into(),
            error,
        }
    }
    pub fn status(&self) -> CaptureStatus {
        CaptureStatus(Arc::new(Mutex::new(self.snapshot(false, false, None))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn work_default_requires_video_keyboard_focus() {
        let mut state = CaptureState::new(Mode::default());
        state.focused = true;
        state.ready = true;
        state.rect = true;
        assert!(state.eligible());
        assert!(!state.captured());
        state.video_focused = true;
        assert!(state.captured());
    }
    #[test]
    fn game_requires_foreground_ready_rect_and_active_session() {
        let mut state = CaptureState::new(Mode::Game);
        state.focused = true;
        state.ready = true;
        state.rect = true;
        assert!(state.captured());
        state.focused = false;
        assert!(!state.captured());
        state.focused = true;
        state.ready = false;
        assert!(!state.captured());
        state.ready = true;
        state.rect = false;
        assert!(!state.captured());
        state.rect = true;
        state.active = false;
        assert!(!state.captured());
    }
    #[test]
    fn release_is_sticky_across_focus_and_reconnect_until_second_chord() {
        let mut state = CaptureState::new(Mode::Game);
        state.focused = true;
        state.ready = true;
        state.rect = true;
        state.toggle();
        assert!(!state.captured());
        state.focused = false;
        state.ready = false;
        state.epoch = 2;
        state.focused = true;
        state.ready = true;
        assert!(!state.captured());
        state.toggle();
        assert!(state.captured());
    }
}
