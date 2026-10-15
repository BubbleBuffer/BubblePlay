//! Fake backend for `--ui-demo`: holds an AppState and a list of demo
//! scenes covering every UI state.
//!
//! The data is copied from `client-win/mockups/connect.html` and
//! `session.html`. Actions change the state the way the real client would,
//! so the demo can be clicked through; there are no timers, so a connect
//! attempt stays in `Connecting` until the user cancels it.

use std::cell::RefCell;
use std::rc::Rc;

use crate::model::{
    Action, Answerer, AnsweredRequest, AppState, Backend, BuildInfo, BuildState, ConnError, ConnStep,
    ConnectedViewer, EnrollStep, Flow, HostRun, HostSetting, HostSettings, HostStatus, Link, Monitor,
    Notifier, Outcome, PairedDevice, PairingRequest, PcKind, PcProblem, Permission, Quality, RemotePc,
    RotatePc, Session, SessionNotice, Stats, StepStatus, StreamMode, ThisDevice, VouchRequest,
};
use crate::ui::{DemoDriver, HomeEdit, HomeSel, SessionUi, StripView, Toast, UiState, View};

// ----------------------------------------------------------- demo data

const THIS_DEVICE_ID: &str = "K9TR-4WQZ-N6EA-3HYC";
const KEY_PATH: &str = r"%APPDATA%\BubblePlay\client\device.bpkey";
const NEW_DEVICE_ID: &str = "P5HM-X2CW-9RTE-D8KJ";
const RELAY: &str = "play.bubblebuffer.net";

const STUDIO_ID: &str = "H4PD-R8TN-B7QK-2M9X";
const LIVING_ID: &str = "Q2WF-7KXM-JD3R-5PNA";
const OFFICE_ID: &str = "Z8TC-M1VE-6HQB-W4LS";
/// A PC paired in the demo, never connected to yet.
const DEN_ID: &str = "R3VK-8MZT-Q1HW-6NCE";
/// The key file the import scenes pick.
const IMPORT_FILE: &str = "studio-laptop.bpkey";

const HOST_ID: &str = "W5NJ-C2RD-9PXF-6KAE";
const REQUEST_ID: &str = "req-m3zd-482913";
const REQUEST_DEVICE: &str = "Work laptop";
const REQUEST_DEVICE_ID: &str = "M3ZD-8QRA-F2KV-T7WB";
const REQUEST_NUMBER: u32 = 482_913;

/// The scene labels, in order. `DemoDriver::len` is `LABELS.len()`.
/// 0..=22 keep the numbering of the snapshots before step 4 (7 was the
/// temporary password step); step 4 scenes follow from 23.
const LABELS: [&str; 47] = [
    "Your PCs",
    "Vouch request",
    "This PC",
    "This PC: request",
    "This PC: answered",
    "Pair: enter code",
    "Pair: confirm",
    "Pair: approved",
    "Approval wait",
    "Connecting",
    "Connect failed",
    "Key refused",
    "Settings",
    "ID rotation",
    "Session: collapsed",
    "Session: toolbar",
    "Session: monitor picker",
    "Session: stats",
    "Session: captured",
    "Session: released",
    "Session: reconnecting",
    "Session: admin required",
    "Session: secure desktop",
    "Your PCs: empty",
    "Your PCs: offline",
    "Your PCs: problems",
    "Your PCs: rename",
    "Your PCs: forget",
    "Set up: new device",
    "Set up: key created",
    "Set up: unlock key file",
    "Set up: import refused",
    "Pair: code refused",
    "Pair: remote request",
    "Pair: remote waiting",
    "Pair: denied",
    "Pair: timed out",
    "Pair: closed at PC",
    "Approval: denied",
    "Approval: timed out",
    "Connect: new PC",
    "Connect: relay unreachable",
    "Connect: PC busy",
    "Connect: connection lost",
    "Connect: no video decoder",
    "Connect: PC key changed",
    "Approval: withdrawn",
];

/// Session sparkline points from session.html (34 px high viewBox, 25
/// points). Converted to frame times below: y 17 is the 16.7 ms line.
const SPARK_Y: [f32; 25] = [
    22.0, 21.0, 23.0, 20.0, 22.0, 21.0, 24.0, 22.0, 21.0, 9.0, 20.0, 22.0, 21.0, 23.0, 22.0, 21.0,
    22.0, 20.0, 22.0, 21.0, 23.0, 22.0, 21.0, 22.0, 21.0,
];

fn studio_sel() -> HomeSel {
    HomeSel::Pc(STUDIO_ID.to_string())
}

fn base_state() -> AppState {
    AppState {
        this_device: ThisDevice { id: THIS_DEVICE_ID.into(), key_path: KEY_PATH.into() },
        pcs: vec![
            RemotePc {
                id: STUDIO_ID.into(),
                name: "Studio PC".into(),
                kind: PcKind::Desktop,
                relay: RELAY.into(),
                online: true,
                last_mode: Some(StreamMode::Game),
                last_seen: Some("Today 09:12".into()),
                allowed: "View and input".into(),
                vouch_request: None,
                problem: None,
                default_monitor: None,
            },
            RemotePc {
                id: LIVING_ID.into(),
                name: "Living room".into(),
                kind: PcKind::Laptop,
                relay: RELAY.into(),
                online: true,
                last_mode: Some(StreamMode::Work),
                last_seen: None,
                allowed: "View and input".into(),
                vouch_request: None,
                problem: None,
                default_monitor: None,
            },
            RemotePc {
                id: OFFICE_ID.into(),
                name: "Office tower".into(),
                kind: PcKind::Desktop,
                relay: RELAY.into(),
                online: false,
                last_mode: None,
                last_seen: Some("2 Oct".into()),
                allowed: "View and input".into(),
                vouch_request: None,
                problem: None,
                default_monitor: None,
            },
        ],
        host: Some(host_base()),
        flow: None,
        session: None,
    }
}

fn host_base() -> HostStatus {
    HostStatus {
        pc_name: "DESKTOP-K9TR".into(),
        pc_id: Some(HOST_ID.into()),
        run: HostRun::Running,
        viewer: Some(ConnectedViewer { name: "Pixel 8".into(), since: "09:12".into() }),
        settings: HostSettings {
            stream_sound: true,
            admin_mode: false,
            start_at_login: true,
            cpu_mode: false,
        },
        build: BuildInfo { hash: "3f9c2e1".into(), state: BuildState::Good },
        can_roll_back: true,
        paired: vec![
            PairedDevice {
                id: "P6XA-91LD".into(),
                name: "Pixel 8".into(),
                connected: true,
                allowed: "View, input".into(),
                last_seen: "Connected since 09:12".into(),
                vouched_by: None,
                may_vouch: true,
            },
            PairedDevice {
                id: "J4HN-0TCV".into(),
                name: "Studio laptop".into(),
                connected: false,
                allowed: "View, input".into(),
                last_seen: "Seen today".into(),
                vouched_by: None,
                may_vouch: false,
            },
            PairedDevice {
                id: "E2YR-5QBW".into(),
                name: "Old tablet".into(),
                connected: false,
                allowed: "View, input".into(),
                last_seen: "Not seen for 41 days".into(),
                vouched_by: Some("Pixel 8".into()),
                may_vouch: false,
            },
        ],
        request: None,
        answered: None,
    }
}

fn pairing_request(expires_in_secs: u32) -> PairingRequest {
    PairingRequest {
        request_id: REQUEST_ID.into(),
        device_name: REQUEST_DEVICE.into(),
        device_id: REQUEST_DEVICE_ID.into(),
        number: REQUEST_NUMBER,
        expires_in_secs,
    }
}

fn vouch_request() -> VouchRequest {
    VouchRequest {
        request_id: REQUEST_ID.into(),
        device_name: REQUEST_DEVICE.into(),
        device_id: REQUEST_DEVICE_ID.into(),
        number: REQUEST_NUMBER,
        expires_in_secs: 521,
        // "Before sign-in" is never offered by vouching, so it is not listed.
        permissions: vec![
            Permission { key: "view".into(), label: "View the screen".into(), granted: true },
            Permission { key: "input".into(), label: "Mouse and keyboard".into(), granted: true },
        ],
    }
}

fn answered_request() -> AnsweredRequest {
    AnsweredRequest {
        request: pairing_request(0),
        answered_on: "Pixel 8".into(),
        allowed: true,
    }
}

fn step(label: &str, detail: &str, status: StepStatus) -> ConnStep {
    ConnStep { label: label.into(), detail: detail.into(), status }
}

fn connecting_steps() -> Vec<ConnStep> {
    vec![
        step("Relay reached", RELAY, StepStatus::Done),
        step("Device key accepted", "K9TR-4WQZ-…", StepStatus::Done),
        step("Stream negotiated", "WebRTC", StepStatus::Done),
        step("Waiting for the first frame", "1.4 s", StepStatus::Now),
    ]
}

fn failed_steps() -> Vec<ConnStep> {
    vec![
        step("Relay reached", RELAY, StepStatus::Done),
        step("Device key accepted", "K9TR-4WQZ-…", StepStatus::Done),
        step("Stream negotiated", "WebRTC", StepStatus::Done),
        step("No first frame", "8.0 s", StepStatus::Failed),
    ]
}

/// The PC the pairing scenes add: paired just now, never connected.
fn den_pc() -> RemotePc {
    RemotePc {
        id: DEN_ID.into(),
        name: "Den PC".into(),
        kind: PcKind::Desktop,
        relay: RELAY.into(),
        online: true,
        last_mode: None,
        last_seen: None,
        allowed: "View and input".into(),
        vouch_request: None,
        problem: None,
        default_monitor: None,
    }
}

fn pair_wait(remote: bool, outcome: Outcome) -> Flow {
    Flow::PairConfirm {
        pc_name: "Den PC".into(),
        pc_id: DEN_ID.into(),
        number: REQUEST_NUMBER,
        waiting_secs: if remote { 96 } else { 18 },
        remote,
        outcome,
    }
}

fn studio_failed(steps: Vec<ConnStep>, error: ConnError) -> Flow {
    Flow::Failed { pc_id: STUDIO_ID.into(), steps, error }
}

fn rotate_pcs() -> Vec<RotatePc> {
    vec![
        RotatePc { name: "Studio PC".into(), status: StepStatus::Done, detail: "confirmed 14:02".into() },
        RotatePc { name: "Living room".into(), status: StepStatus::Done, detail: "confirmed 14:03".into() },
        RotatePc {
            name: "Office tower".into(),
            status: StepStatus::Waiting,
            detail: "offline, confirms when it is back".into(),
        },
    ]
}

fn session_base() -> Session {
    // Frame time in ms: the 16.7 ms line sits at y = 17; a lower point is a
    // faster frame. Gives about 7 ms for the usual line at y = 22 (143.8 fps).
    let frame_ms = SPARK_Y.iter().map(|y| 16.7 + (17.0 - y) * 1.94).collect();
    Session {
        pc_id: STUDIO_ID.into(),
        pc_name: "Studio PC".into(),
        mode: StreamMode::Game,
        rtt_ms: 12,
        monitors: vec![
            Monitor {
                selector: "mock-display-1".into(),
                name: "Display 1".into(),
                width: 2560,
                height: 1440,
                refresh_hz: 144,
                x: 0,
                y: 0,
                primary: true,
            },
            // Desktop positions are approximate, read off the picker map.
            Monitor {
                selector: "mock-display-2".into(),
                name: "Display 2".into(),
                width: 1920,
                height: 1080,
                refresh_hz: 60,
                x: 2646,
                y: 127,
                primary: false,
            },
            Monitor {
                selector: "mock-display-3".into(),
                name: "Display 3".into(),
                width: 1080,
                height: 1920,
                refresh_hz: 60,
                x: -1562,
                y: -212,
                primary: false,
            },
        ],
        monitor: 0,
        monitor_pending: false,
        monitor_error: None,
        quality: Quality::Auto,
        quality_controls_available: true,
        audio: true,
        audio_controls_available: true,
        captured: false,
        link: Link::Live,
        notice: None,
        stats: Stats {
            resolution: "2560 x 1440".into(),
            fps: 143.8,
            bitrate_mbps: 38.2,
            rtt_ms: 12,
            decode_ms: 2.1,
            dropped: 3,
            transport: "WebRTC".into(),
            // The mockup's only codec row is audio ("Opus 10 ms"); no video codec is shown.
            codec: "Opus 10 ms".into(),
            frame_ms,
        },
    }
}

/// The state and UI for scene `index` (0-based, < LABELS.len()).
fn scene_parts(index: usize) -> (AppState, View, SessionUi, Option<HomeEdit>) {
    let mut s = base_state();
    let mut ui = SessionUi::default();
    let mut edit = None;
    let key = "K9TR-4WQZ-…";
    let view = match index {
        0 => View::Home(studio_sel()),
        1 => {
            s.pcs[0].vouch_request = Some(vouch_request());
            View::Home(studio_sel())
        }
        2 => View::Home(HomeSel::ThisPc),
        3 => {
            host_mut(&mut s).request = Some(pairing_request(252));
            View::Home(HomeSel::ThisPc)
        }
        4 => {
            host_mut(&mut s).answered = Some(answered_request());
            View::Home(HomeSel::ThisPc)
        }
        5 => {
            s.flow = Some(Flow::PairEnter { remote: false, error: None });
            View::Home(HomeSel::FirstPc)
        }
        6 => {
            s.flow = Some(Flow::PairConfirm {
                pc_name: "Studio PC".into(),
                pc_id: STUDIO_ID.into(),
                number: REQUEST_NUMBER,
                waiting_secs: 18,
                remote: false,
                outcome: Outcome::Waiting,
            });
            View::Home(HomeSel::FirstPc)
        }
        7 => {
            s.pcs.push(den_pc());
            s.flow = Some(pair_wait(false, Outcome::Approved { by: Answerer::Pc }));
            View::Home(HomeSel::FirstPc)
        }
        8 => {
            s.flow = Some(Flow::Approval { pc_id: STUDIO_ID.into(), waiting_secs: 42, outcome: Outcome::Waiting });
            View::Home(studio_sel())
        }
        9 => {
            s.flow = Some(Flow::Connecting {
                pc_id: STUDIO_ID.into(),
                mode: StreamMode::Game,
                steps: connecting_steps(),
            });
            View::Home(studio_sel())
        }
        10 => {
            s.flow = Some(studio_failed(failed_steps(), ConnError::NoFirstFrame { secs: 8 }));
            View::Home(studio_sel())
        }
        11 => {
            s.flow = Some(Flow::Refused { pc_id: STUDIO_ID.into() });
            View::Home(studio_sel())
        }
        12 => View::Settings,
        13 => {
            s.flow = Some(Flow::Rotate {
                old_id: THIS_DEVICE_ID.into(),
                new_id: NEW_DEVICE_ID.into(),
                pcs: rotate_pcs(),
            });
            View::Settings
        }
        23 => {
            s.pcs.clear();
            s.host = None;
            View::Home(HomeSel::FirstPc)
        }
        24 => View::Home(HomeSel::Pc(OFFICE_ID.into())),
        25 => {
            s.pcs[0].problem = Some(PcProblem::KeyChanged);
            s.pcs[1].problem = Some(PcProblem::NotAccepted);
            s.pcs[2].problem = Some(PcProblem::RelayUnreachable);
            View::Home(studio_sel())
        }
        26 => {
            edit = Some(HomeEdit::Rename(STUDIO_ID.into()));
            View::Home(studio_sel())
        }
        27 => {
            edit = Some(HomeEdit::ConfirmForget(STUDIO_ID.into()));
            View::Home(studio_sel())
        }
        28..=31 => {
            s.pcs.clear();
            s.host = None;
            let step = match index {
                28 => EnrollStep::Choose,
                29 => EnrollStep::Created,
                30 => EnrollStep::ImportUnlock { file_name: IMPORT_FILE.into(), error: None },
                _ => EnrollStep::ImportRefused {
                    file_name: IMPORT_FILE.into(),
                    reason: "The ID in this file doesn't match its key, so it may have been edited. Export it again on the device it came from.".into(),
                },
            };
            if step != EnrollStep::Created {
                s.this_device.id.clear();
            }
            s.flow = Some(Flow::Enroll(step));
            View::Home(HomeSel::FirstPc)
        }
        32 => {
            s.flow = Some(Flow::PairEnter {
                remote: false,
                error: Some("The PC didn't accept that code. It may have expired (codes last 5 minutes) or been used already. Choose Pair device in the tray for a new one.".into()),
            });
            View::Home(HomeSel::FirstPc)
        }
        33 => {
            s.flow = Some(Flow::PairEnter { remote: true, error: None });
            View::Home(HomeSel::FirstPc)
        }
        34 => {
            s.flow = Some(pair_wait(true, Outcome::Waiting));
            View::Home(HomeSel::FirstPc)
        }
        35 => {
            s.flow = Some(pair_wait(false, Outcome::Denied { by: Answerer::Pc }));
            View::Home(HomeSel::FirstPc)
        }
        36 => {
            s.flow = Some(pair_wait(true, Outcome::TimedOut));
            View::Home(HomeSel::FirstPc)
        }
        37 => {
            s.flow = Some(pair_wait(false, Outcome::Cancelled));
            View::Home(HomeSel::FirstPc)
        }
        38 | 39 => {
            let outcome = if index == 38 { Outcome::Denied { by: Answerer::Pc } } else { Outcome::TimedOut };
            s.flow = Some(Flow::Approval { pc_id: STUDIO_ID.into(), waiting_secs: 0, outcome });
            View::Home(studio_sel())
        }
        40 => {
            s.pcs.push(den_pc());
            s.flow = Some(Flow::Connecting { pc_id: DEN_ID.into(), mode: StreamMode::Work, steps: connecting_steps() });
            View::Home(HomeSel::FirstPc)
        }
        41 => {
            s.flow = Some(studio_failed(
                vec![step("Relay", "no answer after 10 s", StepStatus::Failed)],
                ConnError::RelayUnreachable,
            ));
            View::Home(studio_sel())
        }
        42 => {
            s.flow = Some(studio_failed(
                vec![
                    step("Relay reached", RELAY, StepStatus::Done),
                    step("Device key accepted", key, StepStatus::Done),
                    step("PC busy", "another viewer is connected", StepStatus::Failed),
                ],
                ConnError::PcBusy,
            ));
            View::Home(studio_sel())
        }
        43 => {
            s.flow = Some(studio_failed(
                vec![
                    step("Session ran", "41 min", StepStatus::Done),
                    step("Connection dropped", "14:27", StepStatus::Failed),
                    step("Reconnect", "3 attempts, gave up", StepStatus::Failed),
                ],
                ConnError::Lost { attempts: 3 },
            ));
            View::Home(studio_sel())
        }
        44 => {
            s.flow = Some(studio_failed(
                vec![
                    step("Relay reached", RELAY, StepStatus::Done),
                    step("Device key accepted", key, StepStatus::Done),
                    step("Stream negotiated", "WebRTC, H.264", StepStatus::Done),
                    step("Video decoder", "none found", StepStatus::Failed),
                ],
                ConnError::NoHardwareDecoder {
                    codec: "H.264".into(),
                    detail: "Media Foundation: no hardware H.264 decoder on Microsoft Basic Display Adapter".into(),
                },
            ));
            View::Home(studio_sel())
        }
        45 => {
            s.flow = Some(studio_failed(
                vec![
                    step("Relay reached", RELAY, StepStatus::Done),
                    step("PC key checked", "doesn't match", StepStatus::Failed),
                ],
                ConnError::PcKeyChanged,
            ));
            View::Home(studio_sel())
        }
        46 => {
            s.flow = Some(Flow::Approval { pc_id: STUDIO_ID.into(), waiting_secs: 0, outcome: Outcome::Cancelled });
            View::Home(studio_sel())
        }
        _ => {
            // Session scenes (14..=22): the view underneath is never shown.
            let sess = s.session.insert(session_base());
            let view = View::Home(studio_sel());
            match index {
                14 => ui.strip = StripView::Collapsed,
                15 => ui.strip = StripView::Toolbar,
                16 => ui.strip = StripView::MonitorPicker,
                17 => ui.stats_open = true,
                18 => {
                    sess.captured = true;
                    ui.toast = Some(Toast::Captured);
                }
                19 => ui.toast = Some(Toast::Released),
                20 => {
                    sess.link = Link::Reconnecting { attempt: 2, secs: 2 };
                    ui.strip = StripView::Toolbar;
                }
                21 => {
                    sess.notice = Some(SessionNotice::AdminRequired);
                    ui.strip = StripView::Toolbar;
                }
                22 => {
                    sess.notice = Some(SessionNotice::SecureDesktop);
                    ui.strip = StripView::Toolbar;
                }
                _ => unreachable!("scene index is reduced modulo LABELS.len()"),
            }
            view
        }
    };
    (s, view, ui, edit)
}

fn host_mut(s: &mut AppState) -> &mut HostStatus {
    s.host.as_mut().expect("the demo base state always has a host")
}

// ------------------------------------------------------------- backend

#[derive(Default)]
struct Shared {
    state: AppState,
    notify: Option<Notifier>,
}

pub struct MockBackend(Rc<RefCell<Shared>>);
pub struct MockDemo(Rc<RefCell<Shared>>);

pub fn new() -> (MockBackend, MockDemo) {
    let s = Rc::new(RefCell::new(Shared { state: base_state(), notify: None }));
    (MockBackend(s.clone()), MockDemo(s))
}

/// Wakes the window. The notifier is cloned out first so the borrow is
/// released before the callback runs.
fn notify(shared: &Rc<RefCell<Shared>>) {
    let n = shared.borrow().notify.clone();
    if let Some(n) = n {
        n();
    }
}

/// Applies one user action to the mock state. Actions not listed here
/// (key export, logs folder and so on) do nothing in the demo.
fn apply(state: &mut AppState, action: Action) {
    match action {
        Action::Rename { pc_id, name } => {
            if let Some(pc) = state.pcs.iter_mut().find(|p| p.id == pc_id) {
                pc.name = name;
            }
        }
        Action::Forget { pc_id } => {
            state.pcs.retain(|p| p.id != pc_id);
            state.flow = None;
        }
        Action::StartRemotePairing => state.flow = Some(Flow::PairEnter { remote: true, error: None }),
        Action::SubmitPairing { .. } => state.flow = Some(pair_wait(false, Outcome::Waiting)),
        Action::RequestRemotePairing { .. } => state.flow = Some(pair_wait(true, Outcome::Waiting)),
        Action::CreateKey => {
            state.this_device.id = THIS_DEVICE_ID.into();
            state.flow = Some(Flow::Enroll(EnrollStep::Created));
        }
        Action::ImportKey => {
            if matches!(state.flow, Some(Flow::Enroll(_))) {
                state.flow =
                    Some(Flow::Enroll(EnrollStep::ImportUnlock { file_name: IMPORT_FILE.into(), error: None }));
            }
        }
        Action::UnlockImport { passphrase } => {
            if passphrase.is_empty() {
                state.flow = Some(Flow::Enroll(EnrollStep::ImportUnlock {
                    file_name: IMPORT_FILE.into(),
                    error: Some("That passphrase doesn't unlock this file.".into()),
                }));
            } else {
                state.this_device.id = THIS_DEVICE_ID.into();
                state.flow = None;
            }
        }
        Action::Retry => {
            let pc_id = match &state.flow {
                Some(Flow::Failed { pc_id, .. } | Flow::Approval { pc_id, .. }) => Some(pc_id.clone()),
                _ => None,
            };
            if let Some(pc_id) = pc_id {
                apply(state, Action::Connect { pc_id });
            }
        }
        Action::Connect { pc_id } => {
            let mode = state
                .pcs
                .iter()
                .find(|p| p.id == pc_id)
                .and_then(|p| p.last_mode)
                .unwrap_or_default();
            state.flow = Some(Flow::Connecting { pc_id, mode, steps: connecting_steps() });
        }
        Action::CancelFlow => {
            state.flow = match &state.flow {
                // Leaving an import goes back to the first-start choice.
                Some(Flow::Enroll(EnrollStep::ImportUnlock { .. } | EnrollStep::ImportRefused { .. })) => {
                    Some(Flow::Enroll(EnrollStep::Choose))
                }
                _ => None,
            }
        }
        Action::StartPairing => state.flow = Some(Flow::PairEnter { remote: false, error: None }),
        Action::SetHostSetting(setting) => {
            if let Some(host) = state.host.as_mut() {
                let s = &mut host.settings;
                match setting {
                    HostSetting::StreamSound(v) => s.stream_sound = v,
                    HostSetting::AdminMode(v) => s.admin_mode = v,
                    HostSetting::StartAtLogin(v) => s.start_at_login = v,
                    HostSetting::CpuMode(v) => s.cpu_mode = v,
                }
            }
        }
        Action::SetMayVouch { device_id, may_vouch } => {
            if let Some(host) = state.host.as_mut() {
                if let Some(dev) = host.paired.iter_mut().find(|d| d.id == device_id) {
                    dev.may_vouch = may_vouch;
                }
            }
        }
        Action::Revoke { device_id } => {
            if let Some(host) = state.host.as_mut() {
                host.paired.retain(|d| d.id != device_id);
            }
        }
        Action::AnswerPairing { request_id, allow } => {
            if let Some(host) = state.host.as_mut() {
                let is_current = host.request.as_ref().is_some_and(|r| r.request_id == request_id);
                if is_current {
                    if let Some(req) = host.request.take() {
                        if allow {
                            host.paired.push(PairedDevice {
                                id: req.device_id,
                                name: req.device_name,
                                connected: false,
                                allowed: "View, input".into(),
                                last_seen: "Seen today".into(),
                                vouched_by: None,
                                may_vouch: false,
                            });
                        }
                    }
                }
            }
        }
        Action::StopHosting => {
            if let Some(host) = state.host.as_mut() {
                host.run = HostRun::Stopped;
            }
        }
        Action::StartHosting => {
            if let Some(host) = state.host.as_mut() {
                host.run = HostRun::Running;
            }
        }
        Action::SetAudio(on) => {
            if let Some(sess) = state.session.as_mut() {
                sess.audio = on;
            }
        }
        Action::SetMode(mode) => {
            if let Some(sess) = state.session.as_mut() {
                sess.mode = mode;
            }
        }
        Action::SetMonitor(i) => {
            if let Some(sess) = state.session.as_mut() {
                if i < sess.monitors.len() {
                    sess.monitor = i;
                }
            }
        }
        Action::SetQuality(q) => {
            if let Some(sess) = state.session.as_mut() {
                sess.quality = q;
            }
        }
        Action::Disconnect => state.session = None,
        _ => {}
    }
}

impl Backend for MockBackend {
    fn snapshot(&self) -> AppState {
        self.0.borrow().state.clone()
    }
    fn act(&mut self, action: Action) {
        apply(&mut self.0.borrow_mut().state, action);
        notify(&self.0);
    }
    fn set_notifier(&mut self, notify: Notifier) {
        self.0.borrow_mut().notify = Some(notify);
    }
}

impl DemoDriver for MockDemo {
    fn len(&self) -> usize {
        LABELS.len()
    }
    fn scene(&mut self, index: usize) -> (String, UiState) {
        let index = index % LABELS.len();
        let (state, view, session, home_edit) = scene_parts(index);
        self.0.borrow_mut().state = state;
        notify(&self.0);
        (LABELS[index].to_string(), UiState { view, session, home_edit, demo_label: None })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn scenes_apply_without_panic_and_labels_are_unique() {
        let (_backend, mut demo) = new();
        assert_eq!(demo.len(), 47);
        let mut seen = HashSet::new();
        assert_eq!(demo.len(), LABELS.len());
        for (i, expected) in LABELS.iter().enumerate() {
            let (label, _ui) = demo.scene(i);
            assert_eq!(label, *expected);
            assert!(seen.insert(label.clone()), "duplicate scene label: {label}");
        }
        assert_eq!(seen.len(), 47);
    }

    #[test]
    fn toggling_a_host_setting_changes_the_snapshot() {
        let (mut backend, _demo) = new();
        let before = backend.snapshot().host.expect("host").settings;
        backend.act(Action::SetHostSetting(HostSetting::StreamSound(!before.stream_sound)));
        let after = backend.snapshot().host.expect("host").settings;
        assert_ne!(before, after);
        assert_eq!(after.stream_sound, !before.stream_sound);
    }

    #[test]
    fn connect_cancel_and_pairing_answer_update_state() {
        let (mut backend, mut demo) = new();
        backend.act(Action::Connect { pc_id: STUDIO_ID.into() });
        assert!(matches!(backend.snapshot().flow, Some(Flow::Connecting { .. })));
        backend.act(Action::CancelFlow);
        assert!(backend.snapshot().flow.is_none());

        demo.scene(3);
        backend.act(Action::AnswerPairing { request_id: REQUEST_ID.into(), allow: true });
        let host = backend.snapshot().host.expect("host");
        assert!(host.request.is_none());
        assert!(host.paired.iter().any(|d| d.id == REQUEST_DEVICE_ID));
    }

    #[test]
    fn first_start_creates_a_key_and_import_returns_to_the_choice() {
        let (mut backend, mut demo) = new();
        demo.scene(28);
        assert!(backend.snapshot().this_device.id.is_empty());
        backend.act(Action::ImportKey);
        assert!(matches!(backend.snapshot().flow, Some(Flow::Enroll(EnrollStep::ImportUnlock { error: None, .. }))));
        backend.act(Action::UnlockImport { passphrase: String::new() });
        assert!(matches!(backend.snapshot().flow, Some(Flow::Enroll(EnrollStep::ImportUnlock { error: Some(_), .. }))));
        backend.act(Action::CancelFlow);
        assert!(matches!(backend.snapshot().flow, Some(Flow::Enroll(EnrollStep::Choose))));
        backend.act(Action::CreateKey);
        let state = backend.snapshot();
        assert!(matches!(state.flow, Some(Flow::Enroll(EnrollStep::Created))));
        assert_eq!(state.this_device.id, THIS_DEVICE_ID);
    }

    #[test]
    fn rename_forget_and_retry_update_state() {
        let (mut backend, mut demo) = new();
        backend.act(Action::Rename { pc_id: STUDIO_ID.into(), name: "Studio".into() });
        assert_eq!(backend.snapshot().pcs[0].name, "Studio");
        backend.act(Action::Forget { pc_id: STUDIO_ID.into() });
        assert!(backend.snapshot().pcs.iter().all(|p| p.id != STUDIO_ID));

        demo.scene(43);
        backend.act(Action::Retry);
        assert!(matches!(backend.snapshot().flow, Some(Flow::Connecting { .. })));
    }

    #[test]
    fn pairing_requests_lead_to_the_wait() {
        let (mut backend, _demo) = new();
        backend.act(Action::StartRemotePairing);
        assert!(matches!(backend.snapshot().flow, Some(Flow::PairEnter { remote: true, .. })));
        backend.act(Action::RequestRemotePairing { pc_id: DEN_ID.into(), relay: RELAY.into() });
        assert!(matches!(
            backend.snapshot().flow,
            Some(Flow::PairConfirm { remote: true, outcome: Outcome::Waiting, .. })
        ));
    }
}
