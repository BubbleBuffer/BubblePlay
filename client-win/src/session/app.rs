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
    model::{Action, AppState, Backend, Notifier, RemotePc},
    net::SessionControls,
    shell::{FrameRequests, ShellApp, WindowCommand, WindowInfo},
    ui,
};

use crate::stream::control::{overlay_controls, SessionHandles};
use crate::stream::live_input::LiveInputControl;

pub struct SessionBackend {
    /// Media and transport numbers for the session. The controls are overlaid on top.
    pub poll: Arc<dyn Fn() -> AppState + Send + Sync>,
    pub handles: SessionHandles,
    pub stop: Arc<AtomicBool>,
    pub notifier: Option<Notifier>,
}

impl SessionBackend {
    /// Builds the backend for the session of `pc`. The saved default monitor of
    /// `pc` is handed to `controls` here, in memory, so it must run before
    /// `net::run` starts: the network applies it on the first valid list.
    pub fn new(
        poll: Arc<dyn Fn() -> AppState + Send + Sync>,
        controls: SessionControls,
        input: LiveInputControl,
        pc: &RemotePc,
        stop: Arc<AtomicBool>,
    ) -> Self {
        controls.set_default_monitor(pc.default_monitor.clone());
        Self {
            poll,
            handles: SessionHandles {
                controls,
                input,
                pc_id: pc.id.clone(),
                pc_name: pc.name.clone(),
            },
            stop,
            notifier: None,
        }
    }

    fn wake(&self) {
        if let Some(notify) = &self.notifier {
            notify();
        }
    }
}

impl Backend for SessionBackend {
    fn snapshot(&self) -> AppState {
        let mut state = (self.poll)();
        overlay_controls(&mut state, &self.handles);
        state
    }
    fn act(&mut self, action: Action) {
        if crate::stream::control::act(&self.handles, &self.stop, action) {
            self.wake();
        }
    }
    fn set_notifier(&mut self, notify: Notifier) {
        // The net wakes the shell on monitor, pending, error, mode and epoch changes.
        self.handles.controls.set_notifier(notify.clone());
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::PcKind;
    use crate::net::MonitorEntry;
    use std::sync::Mutex;

    use crate::{
        input::Mode,
        model::{Quality, Session, StreamMode},
        net::MonitorError,
        session::live_input::{CaptureFactory, LiveInput},
        shell::{InputCapture, Rect},
    };
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};

    /// Captures nothing. The tests here look at state, not at input.
    struct Idle;

    impl InputCapture for Idle {
        fn on_message(&mut self, _: HWND, _: u32, _: WPARAM, _: LPARAM) -> Option<LRESULT> {
            None
        }
        fn captured(&self) -> bool {
            false
        }
        fn on_frame(&mut self, _: HWND, _: Option<Rect>) {}
    }

    fn no_hwnd() -> HWND {
        HWND(std::ptr::null_mut())
    }

    const EPOCH: &str = "epoch-1";

    /// Controls on a registered `/ws` epoch, as after `hello-ok`. Picks are refused before this.
    fn connected() -> SessionControls {
        let controls = SessionControls::new();
        controls.epoch_registered(EPOCH);
        controls
    }

    fn feed(controls: &SessionControls, list: Vec<MonitorEntry>) {
        controls.apply_monitor_list(list, EPOCH);
    }

    /// A factory that records each mode it builds, in order.
    fn recording(log: Arc<Mutex<Vec<Mode>>>) -> CaptureFactory {
        Box::new(move |mode: Mode| {
            log.lock().unwrap().push(mode);
            (Box::new(Idle) as Box<dyn InputCapture>, None)
        })
    }

    /// The live capture for `mode`, on the same controls the backend uses.
    fn live(mode: Mode, controls: &SessionControls) -> (LiveInput, LiveInputControl) {
        LiveInput::new(
            mode,
            controls.clone(),
            recording(Arc::new(Mutex::new(Vec::new()))),
        )
    }

    fn studio() -> RemotePc {
        RemotePc {
            id: "studio".into(),
            name: "Studio PC".into(),
            ..Default::default()
        }
    }

    fn handles(controls: SessionControls, input: LiveInputControl) -> SessionHandles {
        SessionHandles {
            controls,
            input,
            pc_id: "studio".into(),
            pc_name: "Studio PC".into(),
        }
    }

    /// The production constructor, as the windowed session calls it, for `pc`.
    fn backend_for(
        controls: SessionControls,
        input: LiveInputControl,
        pc: &RemotePc,
    ) -> SessionBackend {
        SessionBackend::new(
            Arc::new(|| AppState {
                session: Some(Session::default()),
                ..Default::default()
            }),
            controls,
            input,
            pc,
            Arc::new(AtomicBool::new(false)),
        )
    }

    fn backend(controls: SessionControls, input: LiveInputControl) -> SessionBackend {
        backend_for(controls, input, &studio())
    }

    /// Monitor 1 is the primary and is selected; monitor 2 sits to its right.
    fn list() -> Vec<MonitorEntry> {
        vec![
            MonitorEntry {
                selector: "mon-1".into(),
                name: "Display 1".into(),
                width: 2560,
                height: 1440,
                origin_x: 0,
                origin_y: 0,
                selected: true,
            },
            MonitorEntry {
                selector: "mon-2".into(),
                name: "Display 2".into(),
                width: 1920,
                height: 1080,
                origin_x: 2560,
                origin_y: 0,
                selected: false,
            },
        ]
    }

    fn session_view(backend: &SessionBackend) -> Session {
        backend.snapshot().session.expect("session")
    }

    #[test]
    fn monitor_list_maps_geometry_selection_and_primary() {
        let controls = connected();
        let (_, input) = live(Mode::Work, &controls);
        let backend = backend(controls.clone(), input);
        feed(&controls, list());

        let session = session_view(&backend);
        assert_eq!(session.monitors.len(), 2);
        assert_eq!(session.monitor, 0);
        assert!(session.monitors[0].primary);
        assert_eq!(session.monitors[1].selector, "mon-2");
        assert_eq!((session.monitors[1].x, session.monitors[1].y), (2560, 0));
        assert!(!session.monitors[1].primary);
        assert_eq!(
            (session.monitors[1].width, session.monitors[1].height),
            (1920, 1080)
        );
        assert!(!session.monitor_pending);
        assert_eq!(session.monitor_error, None);
    }

    #[test]
    fn a_pick_is_pending_and_blocks_a_second_pick_until_answered() {
        let controls = connected();
        let (_, input) = live(Mode::Work, &controls);
        let mut backend = backend(controls.clone(), input);
        feed(&controls, list());

        backend.act(Action::SetMonitor(1));
        assert_eq!(controls.snapshot().pending.as_deref(), Some("mon-2"));
        assert!(session_view(&backend).monitor_pending);

        backend.act(Action::SetMonitor(0));
        assert_eq!(
            controls.snapshot().pending.as_deref(),
            Some("mon-2"),
            "a pick while one is pending is ignored"
        );
        assert_eq!(
            session_view(&backend).monitor,
            0,
            "the selection did not move yet"
        );
    }

    #[test]
    fn picking_the_selected_or_an_unknown_monitor_sends_nothing() {
        let controls = connected();
        let (_, input) = live(Mode::Work, &controls);
        let mut backend = backend(controls.clone(), input);
        feed(&controls, list());

        backend.act(Action::SetMonitor(0));
        backend.act(Action::SetMonitor(9));
        assert_eq!(controls.snapshot().pending, None);
        assert!(!session_view(&backend).monitor_pending);
        assert_eq!(
            backend.snapshot().pcs[0].default_monitor.as_deref(),
            Some("mon-1")
        );
    }

    #[test]
    fn a_monitor_error_is_shown_clears_pending_and_keeps_the_selection() {
        let controls = connected();
        let (_, input) = live(Mode::Work, &controls);
        let mut backend = backend(controls.clone(), input);
        feed(&controls, list());

        backend.act(Action::SetMonitor(1));
        controls.apply_monitor_error(
            MonitorError {
                selector: Some("mon-2".into()),
                reason: "Display 2 is no longer connected".into(),
            },
            EPOCH,
        );

        let session = session_view(&backend);
        assert!(!session.monitor_pending);
        assert_eq!(
            session.monitor_error.as_deref(),
            Some("Display 2 is no longer connected")
        );
        assert_eq!(session.monitor, 0);

        backend.act(Action::SetMonitor(1));
        assert!(
            session_view(&backend).monitor_pending,
            "a new pick is allowed after an error"
        );
    }

    #[test]
    fn a_valid_empty_list_clears_the_monitors_and_none_show_before_the_first_list() {
        let controls = connected();
        let (_, input) = live(Mode::Work, &controls);
        let backend = backend(controls.clone(), input);
        assert!(
            session_view(&backend).monitors.is_empty(),
            "no list yet, so no monitors"
        );

        feed(&controls, list());
        assert_eq!(session_view(&backend).monitors.len(), 2);

        feed(&controls, Vec::new());
        let session = session_view(&backend);
        assert!(
            session.monitors.is_empty(),
            "a valid empty list clears the monitors"
        );
        assert_eq!(session.monitor, 0);
        assert!(!session.monitor_pending);
    }

    #[test]
    fn a_saved_default_is_read_by_the_constructor_and_applied_on_the_first_list() {
        let controls = connected();
        let (_, input) = live(Mode::Work, &controls);
        let saved = RemotePc {
            default_monitor: Some("mon-2".into()),
            ..studio()
        };
        let backend = backend_for(controls.clone(), input, &saved);
        assert_eq!(
            backend.snapshot().pcs[0].default_monitor.as_deref(),
            Some("mon-2")
        );

        feed(&controls, list());
        assert_eq!(
            controls.snapshot().pending.as_deref(),
            Some("mon-2"),
            "the first valid list applies the saved default"
        );
    }

    #[test]
    fn a_user_pick_becomes_the_saved_default_in_memory() {
        let controls = connected();
        let (_, input) = live(Mode::Work, &controls);
        let mut backend = backend(controls.clone(), input);
        feed(&controls, list());

        backend.act(Action::SetMonitor(1));
        let mut accepted = list();
        accepted[0].selected = false;
        accepted[1].selected = true;
        feed(&controls, accepted);

        assert_eq!(
            backend.snapshot().pcs[0].default_monitor.as_deref(),
            Some("mon-2")
        );
        assert_eq!(session_view(&backend).monitor, 1);
    }

    #[test]
    fn the_overlay_keeps_other_pcs_and_the_viewed_pcs_own_fields() {
        let controls = connected();
        controls.initialize_mode(StreamMode::Game);
        let (_, input) = live(Mode::Work, &controls);
        let handles = handles(controls, input);
        let mut state = AppState {
            pcs: vec![
                RemotePc {
                    id: "office".into(),
                    name: "Office PC".into(),
                    last_mode: Some(StreamMode::Work),
                    default_monitor: Some("office-1".into()),
                    last_seen: Some("2 Oct".into()),
                    ..Default::default()
                },
                RemotePc {
                    id: "studio".into(),
                    name: "Old studio name".into(),
                    kind: PcKind::Laptop,
                    online: false,
                    last_mode: Some(StreamMode::Work),
                    default_monitor: Some("stale".into()),
                    last_seen: Some("Today 09:12".into()),
                    allowed: "View and input".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        overlay_controls(&mut state, &handles);

        assert_eq!(state.pcs.len(), 2);
        let office = &state.pcs[0];
        assert_eq!(
            (office.id.as_str(), office.name.as_str()),
            ("office", "Office PC")
        );
        assert_eq!(office.last_mode, Some(StreamMode::Work));
        assert_eq!(office.default_monitor.as_deref(), Some("office-1"));
        assert_eq!(office.last_seen.as_deref(), Some("2 Oct"));

        let studio = &state.pcs[1];
        assert_eq!(studio.name, "Old studio name", "the stored name is kept");
        assert!(
            matches!(studio.kind, PcKind::Laptop),
            "the stored kind is kept"
        );
        assert!(!studio.online, "the stored online flag is kept");
        assert_eq!(studio.allowed, "View and input");
        assert_eq!(studio.last_seen.as_deref(), Some("Today 09:12"));
        assert_eq!(
            studio.last_mode,
            Some(StreamMode::Game),
            "the viewed PC takes the mode"
        );
        assert_eq!(
            studio.default_monitor, None,
            "the viewed PC takes the default"
        );
    }

    #[test]
    fn the_viewed_pc_is_appended_when_the_state_lacks_it() {
        let controls = connected();
        let (_, input) = live(Mode::Work, &controls);
        let mut state = AppState {
            pcs: vec![RemotePc {
                id: "office".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        overlay_controls(&mut state, &handles(controls, input));

        let ids: Vec<&str> = state.pcs.iter().map(|pc| pc.id.as_str()).collect();
        assert_eq!(ids, ["office", "studio"]);
        assert_eq!(state.pcs[1].name, "Studio PC");
        assert_eq!(state.pcs[1].last_mode, Some(StreamMode::Work));
        assert!(state.pcs[1].online);
    }

    #[test]
    fn set_mode_reaches_the_net_now_and_the_capture_only_after_the_write() {
        let controls = connected();
        controls.initialize_mode(StreamMode::Work);
        let installed = Arc::new(Mutex::new(Vec::new()));
        let (mut capture, input) =
            LiveInput::new(Mode::Work, controls.clone(), recording(installed.clone()));
        let mut backend = backend(controls.clone(), input.clone());

        backend.act(Action::SetMode(StreamMode::Game));
        assert_eq!(controls.snapshot().mode, StreamMode::Game);
        assert_eq!(session_view(&backend).mode, StreamMode::Game);
        assert_eq!(input.desired_mode(), Mode::Game);

        // The Game command is queued, not written: no Game capture may build yet.
        capture.on_frame(no_hwnd(), None);
        assert_eq!(*installed.lock().unwrap(), [Mode::Work]);

        let outbound = controls.take_outbound(EPOCH);
        assert_eq!(outbound.mode, Some(StreamMode::Game));
        controls.mark_mode_written(outbound.mode_request);
        capture.on_frame(no_hwnd(), None);
        assert_eq!(*installed.lock().unwrap(), [Mode::Work, Mode::Game]);

        backend.act(Action::SetMode(StreamMode::Game));
        capture.on_frame(no_hwnd(), None);
        assert_eq!(
            *installed.lock().unwrap(),
            [Mode::Work, Mode::Game],
            "the same mode again changes nothing"
        );
    }

    #[test]
    fn the_shell_wake_is_registered_with_the_net_controls() {
        let controls = connected();
        let input = live(Mode::Work, &controls).1;
        let mut backend = backend(controls.clone(), input);
        let wakes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = wakes.clone();
        backend.set_notifier(Arc::new(move || {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }));

        // A net-side change, not a UI action, must wake the shell.
        controls.epoch_restart();
        assert_eq!(wakes.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn quality_and_audio_stay_unavailable_in_the_live_session() {
        let controls = connected();
        let (_, input) = live(Mode::Work, &controls);
        let mut backend = backend(controls, input);

        backend.act(Action::SetQuality(Quality::Sharp));
        backend.act(Action::SetAudio(true));
        let session = session_view(&backend);
        assert_eq!(session.quality, Quality::Auto);
        assert!(!session.audio);
        assert!(!session.quality_controls_available);
        assert!(!session.audio_controls_available);
    }

    #[test]
    fn disconnect_still_stops_the_session() {
        let controls = connected();
        let (_, input) = live(Mode::Work, &controls);
        let mut backend = backend(controls, input);
        backend.act(Action::Disconnect);
        assert!(backend.stop.load(Ordering::Acquire));
    }
}
