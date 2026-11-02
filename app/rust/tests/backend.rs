//! The real backend against a throwaway viewer directory (the key is wrapped with the real
//! Windows data protection, as in the app).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use bp_device_key::{DeviceId, KeyFile};
use bubbleplay_app_core::backend::{AppBackend, Paths};
use bubbleplay_app_core::contract::Core;
use bubbleplay_client::model::{Action, AppState, Backend, EnrollStep, Flow, StreamMode};
use bubbleplay_client::store::pcs::PcStore;

struct Dir(PathBuf);

impl Dir {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!("bp-app-core-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn backend(&self) -> AppBackend {
        AppBackend::new(Paths::in_dir(&self.0))
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn plain_key_text() -> (String, String) {
    let key = KeyFile::create("test viewer", "2026-10-30T00:00:00Z", None).unwrap();
    (key.to_json().unwrap(), key.id().to_string())
}

fn wait_for(backend: &AppBackend, what: &str, ready: impl Fn(&AppState) -> bool) -> AppState {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let state = backend.snapshot();
        if ready(&state) {
            return state;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_fresh_install_asks_to_create_or_import_a_key() {
    let dir = Dir::new("fresh");
    let state = dir.backend().snapshot();
    assert!(state.this_device.id.is_empty());
    assert!(matches!(state.flow, Some(Flow::Enroll(EnrollStep::Choose))));
    assert!(state.pcs.is_empty());
    assert!(state.host.is_none());
}

#[test]
fn create_key_shows_the_id_and_the_next_start_needs_no_flow() {
    let dir = Dir::new("create");
    let mut backend = dir.backend();
    backend.act(Action::CreateKey);
    let state = backend.snapshot();
    assert!(matches!(
        state.flow,
        Some(Flow::Enroll(EnrollStep::Created))
    ));
    let id = state.this_device.id.clone();
    assert_eq!(id.len(), 19, "four groups of four: {id}");

    backend.act(Action::CancelFlow);
    assert!(backend.snapshot().flow.is_none());

    let reopened = dir.backend().snapshot();
    assert!(reopened.flow.is_none());
    assert_eq!(reopened.this_device.id, id);

    // The export is the portable file of the same key.
    let exported = dir.backend().export_key().unwrap();
    assert_eq!(KeyFile::parse(&exported).unwrap().id().to_string(), id);
}

#[test]
fn a_second_create_never_replaces_the_key() {
    let dir = Dir::new("twice");
    let mut backend = dir.backend();
    backend.act(Action::CreateKey);
    let id = backend.snapshot().this_device.id;
    backend.act(Action::CancelFlow);
    backend.act(Action::CreateKey);
    assert_eq!(backend.snapshot().this_device.id, id);
}

#[test]
fn importing_a_plain_key_file_goes_straight_to_the_home_screen() {
    let dir = Dir::new("import-plain");
    let (text, id) = plain_key_text();
    let mut backend = dir.backend();
    backend.act(Action::ImportKeyText {
        file_name: "viewer.bpkey".into(),
        text,
    });
    let state = backend.snapshot();
    assert!(state.flow.is_none());
    assert_eq!(state.this_device.id, id);
}

#[test]
fn a_file_that_is_not_a_key_is_refused_and_cancel_returns_to_the_choice() {
    let dir = Dir::new("import-bad");
    let mut backend = dir.backend();
    backend.act(Action::ImportKeyText {
        file_name: "notes.txt".into(),
        text: "hello".into(),
    });
    match backend.snapshot().flow {
        Some(Flow::Enroll(EnrollStep::ImportRefused { file_name, reason })) => {
            assert_eq!(file_name, "notes.txt");
            assert!(!reason.is_empty());
        }
        other => panic!("expected ImportRefused, got {other:?}"),
    }
    backend.act(Action::CancelFlow);
    assert!(matches!(
        backend.snapshot().flow,
        Some(Flow::Enroll(EnrollStep::Choose))
    ));
}

#[test]
fn a_protected_key_file_asks_for_its_passphrase_once_and_is_stored_without_it() {
    let dir = Dir::new("import-protected");
    let key = KeyFile::create("protected", "2026-10-30T00:00:00Z", Some("correct horse")).unwrap();
    let mut backend = dir.backend();
    backend.act(Action::ImportKeyText {
        file_name: "protected.bpkey".into(),
        text: key.to_json().unwrap(),
    });
    assert!(matches!(
        backend.snapshot().flow,
        Some(Flow::Enroll(EnrollStep::ImportUnlock { error: None, .. }))
    ));

    backend.act(Action::UnlockImport {
        passphrase: "wrong".into(),
    });
    match backend.snapshot().flow {
        Some(Flow::Enroll(EnrollStep::ImportUnlock { error, file_name })) => {
            assert!(error.is_some());
            assert_eq!(file_name, "protected.bpkey");
        }
        other => panic!("expected ImportUnlock with an error, got {other:?}"),
    }
    assert!(backend.snapshot().this_device.id.is_empty());

    backend.act(Action::UnlockImport {
        passphrase: "correct horse".into(),
    });
    let state = backend.snapshot();
    assert!(state.flow.is_none());
    assert_eq!(state.this_device.id, key.id().to_string());
    // Stored plain (the data protection of the account keeps it): a restart needs no passphrase.
    assert!(dir.backend().snapshot().flow.is_none());
}

#[test]
fn pairing_input_is_checked_before_anything_is_sent() {
    let dir = Dir::new("pair-input");
    let mut backend = dir.backend();
    backend.act(Action::CreateKey);
    backend.act(Action::CancelFlow);
    backend.act(Action::StartPairing);
    assert!(matches!(
        backend.snapshot().flow,
        Some(Flow::PairEnter {
            remote: false,
            error: None
        })
    ));

    let submit = |backend: &mut AppBackend, pc: &str, code: &str, relay: &str| {
        backend.act(Action::SubmitPairing {
            link: None,
            pc_id: pc.into(),
            code: code.into(),
            relay: relay.into(),
        });
        match backend.snapshot().flow {
            Some(Flow::PairEnter { error: Some(e), .. }) => e,
            other => panic!("expected a PairEnter error, got {other:?}"),
        }
    };
    assert!(submit(&mut backend, "nope", "70Q1K-9MXR1", "https://relay.example").contains("PC ID"));
    assert!(submit(
        &mut backend,
        "77VH-7M56-8GJK-Y12J",
        "xx",
        "https://relay.example"
    )
    .contains("pairing code"));
    assert!(submit(&mut backend, "77VH-7M56-8GJK-Y12J", "70Q1K-9MXR1", " ").contains("relay"));
}

#[test]
fn an_unreachable_relay_returns_to_the_form_with_a_reason() {
    let dir = Dir::new("pair-unreachable");
    let mut backend = dir.backend();
    backend.act(Action::CreateKey);
    backend.act(Action::CancelFlow);
    backend.act(Action::SubmitPairing {
        link: Some("bubbleplay://pair?relay=http%3A%2F%2F127.0.0.1%3A9&pc=77VH-7M56-8GJK-Y12J&code=70Q1K-9MXR1".into()),
        pc_id: String::new(),
        code: String::new(),
        relay: String::new(),
    });
    assert!(matches!(
        backend.snapshot().flow,
        Some(Flow::PairConfirm { .. })
    ));
    let state = wait_for(&backend, "the pairing to fail", |s| {
        matches!(s.flow, Some(Flow::PairEnter { .. }))
    });
    match state.flow {
        Some(Flow::PairEnter {
            error: Some(error), ..
        }) => assert!(error.contains("relay"), "{error}"),
        other => panic!("{other:?}"),
    }
    assert!(state.pcs.is_empty());
}

#[test]
fn cancelling_a_pairing_drops_its_late_result() {
    let dir = Dir::new("pair-cancel");
    let mut backend = dir.backend();
    backend.act(Action::CreateKey);
    backend.act(Action::CancelFlow);
    backend.act(Action::SubmitPairing {
        link: None,
        pc_id: "77VH-7M56-8GJK-Y12J".into(),
        code: "70Q1K-9MXR1".into(),
        relay: "http://127.0.0.1:9".into(),
    });
    backend.act(Action::CancelFlow);
    std::thread::sleep(Duration::from_secs(3));
    assert!(backend.snapshot().flow.is_none());
}

#[test]
fn connect_starts_a_session_or_says_why_not_and_disconnect_closes_it() {
    let dir = Dir::new("connect");
    let id = DeviceId::from_public_key(&[7u8; 32]);
    PcStore::in_dir(&dir.0)
        .pin(&id, &[7u8; 32], "https://relay.example")
        .unwrap();
    let mut backend = dir.backend();
    backend.act(Action::CreateKey);
    backend.act(Action::CancelFlow);
    let state = backend.snapshot();
    assert_eq!(state.pcs.len(), 1);
    assert_eq!(state.pcs[0].id, id.to_string());

    backend.act(Action::Rename {
        pc_id: id.to_string(),
        name: "Studio".into(),
    });
    backend.act(Action::Connect {
        pc_id: id.to_string(),
    });
    // The real session needs a hardware decoder and a reachable relay: without them Connect
    // reports why (a failed flow and a problem on the PC) instead of opening a session.
    let state = backend.snapshot();
    match state.session {
        Some(session) => {
            assert_eq!(session.pc_name, "Studio");
            assert_eq!(session.mode, StreamMode::Work);
        }
        None => {
            assert!(
                matches!(state.flow, Some(Flow::Failed { .. } | Flow::Refused { .. })),
                "no session and no reason: {:?}",
                state.flow
            );
        }
    }
    backend.act(Action::Disconnect);
    assert!(backend.snapshot().session.is_none());

    backend.act(Action::Forget {
        pc_id: id.to_string(),
    });
    assert!(backend.snapshot().pcs.is_empty());
}

#[test]
fn the_json_contract_carries_snapshots_and_actions() {
    let dir = Dir::new("json");
    let mut core = Core::new(dir.backend());
    let state: AppState = serde_json::from_str(&core.snapshot_json()).unwrap();
    assert!(matches!(state.flow, Some(Flow::Enroll(EnrollStep::Choose))));

    core.act_json(r#""CreateKey""#).unwrap();
    let snapshot: serde_json::Value = serde_json::from_str(&core.snapshot_json()).unwrap();
    assert_eq!(snapshot["flow"]["Enroll"], "Created");

    assert!(core.act_json(r#"{"Nope":1}"#).is_err());
    core.act_json(r#"{"ImportKeyText":{"file_name":"a","text":"b"}}"#)
        .unwrap();
}
