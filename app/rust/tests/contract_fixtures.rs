//! Writes (and checks) the contract fixtures the Dart models are tested against:
//! `app/test/fixtures/contract/{states,actions}.json`. Every `Flow`, `EnrollStep`, `Outcome`,
//! `ConnError` and `Action` variant appears at least once, so a Rust change to the model that
//! the Dart side has not followed fails `flutter test` and a Dart-only change fails here.
//!
//! `UPDATE_FIXTURES=1 cargo test --test contract_fixtures` rewrites the files.

use std::collections::BTreeMap;
use std::path::PathBuf;

use bubbleplay_client::model::*;
use serde_json::{json, Value};

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../test/fixtures/contract")
}

fn pc(id: &str, name: &str) -> RemotePc {
    RemotePc {
        id: id.into(),
        name: name.into(),
        kind: PcKind::Desktop,
        relay: "https://relay.example".into(),
        online: true,
        last_mode: Some(StreamMode::Game),
        last_seen: Some("Today 09:12".into()),
        allowed: "View and input".into(),
        vouch_request: None,
        problem: None,
        default_monitor: Some("DISPLAY1".into()),
    }
}

fn steps() -> Vec<ConnStep> {
    vec![
        ConnStep {
            label: "Reach relay".into(),
            detail: "relay.example".into(),
            status: StepStatus::Done,
        },
        ConnStep {
            label: "Wait for first frame".into(),
            detail: String::new(),
            status: StepStatus::Now,
        },
        ConnStep {
            label: "Start stream".into(),
            detail: String::new(),
            status: StepStatus::Waiting,
        },
    ]
}

fn base() -> AppState {
    AppState {
        this_device: ThisDevice {
            id: "AB12-CD34-EF56-GH78".into(),
            key_path: "C:\\Users\\me\\AppData\\Local\\BubblePlay\\viewer\\device-key.bpkey".into(),
        },
        pcs: vec![pc("77VH-7M56-8GJK-Y12J", "Studio")],
        ..AppState::default()
    }
}

fn with_flow(flow: Flow) -> AppState {
    AppState {
        flow: Some(flow),
        ..base()
    }
}

fn pair_confirm(remote: bool, outcome: Outcome) -> Flow {
    Flow::PairConfirm {
        pc_name: "Studio".into(),
        pc_id: "77VH-7M56-8GJK-Y12J".into(),
        number: 482913,
        waiting_secs: 42,
        remote,
        outcome,
    }
}

fn states() -> BTreeMap<&'static str, AppState> {
    let mut map = BTreeMap::new();
    let mut put = |name, state| {
        map.insert(name, state);
    };
    put(
        "first_run",
        AppState {
            flow: Some(Flow::Enroll(EnrollStep::Choose)),
            ..AppState::default()
        },
    );
    put("home", base());
    put("home_empty", AppState {
        this_device: base().this_device,
        ..AppState::default()
    });
    let mut problems = base();
    problems.pcs = vec![
        RemotePc {
            problem: Some(PcProblem::RelayUnreachable),
            online: false,
            ..pc("11AA-22BB-33CC-44DD", "Relay down")
        },
        RemotePc {
            problem: Some(PcProblem::NotAccepted),
            last_mode: None,
            last_seen: None,
            default_monitor: None,
            kind: PcKind::Laptop,
            ..pc("55EE-66FF-77GG-88HH", "Laptop")
        },
        RemotePc {
            problem: Some(PcProblem::KeyChanged),
            vouch_request: Some(VouchRequest {
                request_id: "r1".into(),
                device_name: "Phone".into(),
                device_id: "9JJK-MMNN-PPQQ-RRSS".into(),
                number: 123456,
                expires_in_secs: 240,
                permissions: vec![
                    Permission {
                        key: "view".into(),
                        label: "View".into(),
                        granted: true,
                    },
                    Permission {
                        key: "input".into(),
                        label: "Input".into(),
                        granted: false,
                    },
                ],
            }),
            ..pc("TTVV-WWXX-YYZZ-0011", "Office")
        },
    ];
    put("home_problems", problems);
    let mut host = base();
    host.host = Some(HostStatus {
        pc_name: "DESKTOP-1".into(),
        pc_id: Some("1234-5678-9ABC-DEFG".into()),
        run: HostRun::Restarting { in_secs: 5 },
        viewer: Some(ConnectedViewer {
            name: "Phone".into(),
            since: "09:12".into(),
        }),
        settings: HostSettings {
            stream_sound: true,
            admin_mode: false,
            start_at_login: true,
            cpu_mode: false,
        },
        build: BuildInfo {
            hash: "abc1234".into(),
            state: BuildState::Probation,
        },
        can_roll_back: true,
        paired: vec![PairedDevice {
            id: "9JJK-MMNN-PPQQ-RRSS".into(),
            name: "Phone".into(),
            connected: true,
            allowed: "View, input".into(),
            last_seen: "Seen today".into(),
            vouched_by: Some("Laptop".into()),
            may_vouch: true,
        }],
        request: Some(PairingRequest {
            request_id: "req".into(),
            device_name: "Tablet".into(),
            device_id: "ABCD-EFGH-JKMN-PQRS".into(),
            number: 654321,
            expires_in_secs: 200,
        }),
        answered: Some(AnsweredRequest {
            request: PairingRequest::default(),
            answered_on: "the tray".into(),
            allowed: true,
        }),
    });
    put("home_with_host", host);
    for (name, run) in [
        ("host_starting", HostRun::Starting),
        ("host_running", HostRun::Running),
        ("host_stopped", HostRun::Stopped),
        ("host_not_configured", HostRun::NotConfigured),
    ] {
        let mut state = base();
        state.host = Some(HostStatus {
            run,
            ..HostStatus::default()
        });
        put(name, state);
    }

    put(
        "enroll_created",
        with_flow(Flow::Enroll(EnrollStep::Created)),
    );
    put(
        "enroll_unlock",
        with_flow(Flow::Enroll(EnrollStep::ImportUnlock {
            file_name: "viewer.bpkey".into(),
            error: Some("That passphrase did not open the key.".into()),
        })),
    );
    put(
        "enroll_unlock_clean",
        with_flow(Flow::Enroll(EnrollStep::ImportUnlock {
            file_name: "viewer.bpkey".into(),
            error: None,
        })),
    );
    put(
        "enroll_refused",
        with_flow(Flow::Enroll(EnrollStep::ImportRefused {
            file_name: "notes.txt".into(),
            reason: "This is not a BubblePlay key file.".into(),
        })),
    );
    put(
        "pair_enter",
        with_flow(Flow::PairEnter {
            remote: false,
            error: None,
        }),
    );
    put(
        "pair_enter_remote_error",
        with_flow(Flow::PairEnter {
            remote: true,
            error: Some("Remote pairing is not available in this version.".into()),
        }),
    );
    put("pair_waiting", with_flow(pair_confirm(false, Outcome::Waiting)));
    put(
        "pair_approved_pc",
        with_flow(pair_confirm(false, Outcome::Approved { by: Answerer::Pc })),
    );
    put(
        "pair_approved_device",
        with_flow(pair_confirm(
            true,
            Outcome::Approved {
                by: Answerer::Device("Laptop".into()),
            },
        )),
    );
    put(
        "pair_denied",
        with_flow(pair_confirm(false, Outcome::Denied { by: Answerer::Pc })),
    );
    put("pair_timed_out", with_flow(pair_confirm(false, Outcome::TimedOut)));
    put("pair_cancelled", with_flow(pair_confirm(false, Outcome::Cancelled)));
    put(
        "approval",
        with_flow(Flow::Approval {
            pc_id: "77VH-7M56-8GJK-Y12J".into(),
            waiting_secs: 12,
            outcome: Outcome::Waiting,
        }),
    );
    put(
        "connecting",
        with_flow(Flow::Connecting {
            pc_id: "77VH-7M56-8GJK-Y12J".into(),
            mode: StreamMode::Work,
            steps: steps(),
        }),
    );
    let errors = [
        ("failed_no_first_frame", ConnError::NoFirstFrame { secs: 15 }),
        ("failed_relay", ConnError::RelayUnreachable),
        ("failed_busy", ConnError::PcBusy),
        ("failed_lost", ConnError::Lost { attempts: 5 }),
        (
            "failed_decoder",
            ConnError::NoHardwareDecoder {
                codec: "hevc".into(),
                detail: "no hardware decoder".into(),
            },
        ),
        ("failed_key_changed", ConnError::PcKeyChanged),
    ];
    for (name, error) in errors {
        put(
            name,
            with_flow(Flow::Failed {
                pc_id: "77VH-7M56-8GJK-Y12J".into(),
                steps: steps(),
                error,
            }),
        );
    }
    put(
        "refused",
        with_flow(Flow::Refused {
            pc_id: "77VH-7M56-8GJK-Y12J".into(),
        }),
    );
    put(
        "rotate",
        with_flow(Flow::Rotate {
            old_id: "AB12-CD34-EF56-GH78".into(),
            new_id: "ZZ99-YY88-XX77-WW66".into(),
            pcs: vec![
                RotatePc {
                    name: "Studio".into(),
                    status: StepStatus::Done,
                    detail: "Updated".into(),
                },
                RotatePc {
                    name: "Office".into(),
                    status: StepStatus::Failed,
                    detail: "Offline".into(),
                },
            ],
        }),
    );

    let mut session = base();
    session.session = Some(Session {
        pc_id: "77VH-7M56-8GJK-Y12J".into(),
        pc_name: "Studio".into(),
        mode: StreamMode::Game,
        rtt_ms: 18,
        monitors: vec![
            Monitor {
                selector: "DISPLAY1".into(),
                name: "Display 1".into(),
                width: 2560,
                height: 1440,
                refresh_hz: 144,
                x: 0,
                y: 0,
                primary: true,
            },
            Monitor {
                selector: "DISPLAY2".into(),
                name: "Display 2".into(),
                width: 1920,
                height: 1080,
                refresh_hz: 60,
                x: -1920,
                y: 0,
                primary: false,
            },
        ],
        monitor: 1,
        monitor_pending: true,
        monitor_error: Some("The PC could not switch displays.".into()),
        quality: Quality::Sharp,
        quality_controls_available: true,
        audio: true,
        audio_controls_available: true,
        captured: true,
        link: Link::Reconnecting {
            attempt: 2,
            secs: 4,
        },
        notice: Some(SessionNotice::SecureDesktop),
        stats: Stats {
            resolution: "2560x1440".into(),
            fps: 59.5,
            bitrate_mbps: 24.5,
            rtt_ms: 18,
            decode_ms: 2.5,
            dropped: 3,
            transport: "WebRTC".into(),
            codec: "H.264".into(),
            frame_ms: vec![16.5, 16.75, 17.0],
        },
    });
    put("session", session);
    let mut live = base();
    live.session = Some(Session {
        pc_id: "77VH-7M56-8GJK-Y12J".into(),
        pc_name: "Studio".into(),
        notice: Some(SessionNotice::AdminRequired),
        ..Session::default()
    });
    put("session_placeholder", live);
    map
}

fn actions() -> BTreeMap<&'static str, Action> {
    let mut map = BTreeMap::new();
    let mut put = |name, action| {
        map.insert(name, action);
    };
    let id = || "77VH-7M56-8GJK-Y12J".to_owned();
    put("connect", Action::Connect { pc_id: id() });
    put("cancel_flow", Action::CancelFlow);
    put("retry", Action::Retry);
    put("retry_web_socket", Action::RetryWebSocket);
    put("forget", Action::Forget { pc_id: id() });
    put(
        "rename",
        Action::Rename {
            pc_id: id(),
            name: "Studio".into(),
        },
    );
    put("start_pairing", Action::StartPairing);
    put("start_remote_pairing", Action::StartRemotePairing);
    put(
        "submit_pairing",
        Action::SubmitPairing {
            link: None,
            pc_id: id(),
            code: "70Q1K-9MXR1".into(),
            relay: "https://relay.example".into(),
        },
    );
    put(
        "submit_pairing_link",
        Action::SubmitPairing {
            link: Some("bubbleplay://pair?relay=x&pc=y&code=z".into()),
            pc_id: String::new(),
            code: String::new(),
            relay: String::new(),
        },
    );
    put(
        "request_remote_pairing",
        Action::RequestRemotePairing {
            pc_id: id(),
            relay: "https://relay.example".into(),
        },
    );
    put(
        "vouch",
        Action::Vouch {
            request_id: "r1".into(),
            accept: true,
            permissions: vec!["view".into()],
        },
    );
    put(
        "answer_pairing",
        Action::AnswerPairing {
            request_id: "req".into(),
            allow: false,
        },
    );
    put(
        "revoke",
        Action::Revoke {
            device_id: id(),
        },
    );
    put(
        "set_may_vouch",
        Action::SetMayVouch {
            device_id: id(),
            may_vouch: true,
        },
    );
    put(
        "set_host_setting_stream_sound",
        Action::SetHostSetting(HostSetting::StreamSound(true)),
    );
    put(
        "set_host_setting_admin_mode",
        Action::SetHostSetting(HostSetting::AdminMode(false)),
    );
    put(
        "set_host_setting_start_at_login",
        Action::SetHostSetting(HostSetting::StartAtLogin(true)),
    );
    put(
        "set_host_setting_cpu_mode",
        Action::SetHostSetting(HostSetting::CpuMode(true)),
    );
    put("start_hosting", Action::StartHosting);
    put("stop_hosting", Action::StopHosting);
    put("restart_host", Action::RestartHost);
    put("roll_back_host", Action::RollBackHost);
    put("open_logs_folder", Action::OpenLogsFolder);
    put("show_pair_device", Action::ShowPairDevice);
    put("create_key", Action::CreateKey);
    put(
        "unlock_import",
        Action::UnlockImport {
            passphrase: "secret".into(),
        },
    );
    put("export_key", Action::ExportKey);
    put("import_key", Action::ImportKey);
    put(
        "import_key_text",
        Action::ImportKeyText {
            file_name: "viewer.bpkey".into(),
            text: "{}".into(),
        },
    );
    put("replace_key", Action::ReplaceKey);
    put("set_mode", Action::SetMode(StreamMode::Game));
    put("set_monitor", Action::SetMonitor(1));
    put("set_quality", Action::SetQuality(Quality::Smooth));
    put("set_audio", Action::SetAudio(true));
    put("disconnect", Action::Disconnect);
    map
}

fn check(name: &str, value: Value) {
    let path = dir().join(name);
    let text = serde_json::to_string_pretty(&value).unwrap() + "\n";
    if std::env::var_os("UPDATE_FIXTURES").is_some() {
        std::fs::create_dir_all(dir()).unwrap();
        std::fs::write(&path, text).unwrap();
        return;
    }
    let stored = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("{} is missing: run with UPDATE_FIXTURES=1", path.display()));
    assert_eq!(
        stored.replace("\r\n", "\n"),
        text,
        "{name} is out of date: run with UPDATE_FIXTURES=1 and commit it"
    );
}

#[test]
fn state_fixtures_match_the_rust_model() {
    let mut object = serde_json::Map::new();
    for (name, state) in states() {
        let value = serde_json::to_value(&state).unwrap();
        // The model must read back what it writes.
        let back: AppState = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(&back).unwrap(), value, "{name}");
        object.insert(name.to_owned(), value);
    }
    check("states.json", Value::Object(object));
}

#[test]
fn action_fixtures_match_the_rust_model() {
    let mut object = serde_json::Map::new();
    for (name, action) in actions() {
        let value = serde_json::to_value(&action).unwrap();
        let back: Action = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(back, action, "{name}");
        object.insert(name.to_owned(), value);
    }
    check("actions.json", Value::Object(object));
    let _ = json!(null);
}
