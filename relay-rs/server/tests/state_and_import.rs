//! `import-go-auth`, the state store and `init`, end to end on temp directories.

use std::fs;
use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bp_device_key::DeviceId;
use bp_relay_server::config::load_config;
use bp_relay_server::import::{ImportError, import_go_auth, import_go_auth_file};
use bp_relay_server::init::write_example_config;
use bp_relay_server::store::{self, PCS_FILE, PcRecord, PcsFile, StoreError};

fn public_b64(seed: u8) -> String {
    URL_SAFE_NO_PAD.encode([seed; 32])
}

fn id_for(seed: u8) -> String {
    DeviceId::from_public_key(&[seed; 32]).to_string()
}

/// A Go-format auth.json with one valid PC, one PC whose ID belongs to another key, one bad
/// public key, one non-canonical key, plus the secret-bearing sections that must be ignored.
fn go_auth_text() -> String {
    let mismatched_id = id_for(9);
    serde_json::json!({
        "version": 1,
        "configuredPasswordHash": "scrypt$not-a-real-hash",
        "passwordHash": "scrypt$not-a-real-hash",
        "factorTag": "tag",
        "totpTag": "tag",
        "lastTotpCounter": 42,
        "gates": {"g1": {"id": "x", "expiresAt": 1}},
        "sessions": {"s1": {"id": "y", "expiresAt": 1}},
        "rates": {},
        "pcs": {
            id_for(1): {"public": public_b64(1), "firstSeen": 1_700_000_000_001i64, "lastSeen": 1_700_000_000_002i64},
            mismatched_id: {"public": public_b64(3), "firstSeen": 5, "lastSeen": 6},
            id_for(4): {"public": "not base64 at all!", "firstSeen": 5, "lastSeen": 6},
            id_for(5).to_lowercase(): {"public": public_b64(5), "firstSeen": 5, "lastSeen": 6},
            id_for(6): {"public": public_b64(6), "firstSeen": 0, "lastSeen": 0}
        }
    })
    .to_string()
}

fn read_pcs(dir: &Path) -> PcsFile {
    store::load_pcs(dir).expect("pcs.json loads")
}

#[test]
fn import_keeps_only_valid_pcs_and_reports_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let report = import_go_auth(dir.path(), &go_auth_text()).expect("import runs");
    assert_eq!(report.imported, 1);
    assert_eq!(report.already_present, 0);
    assert_eq!(report.skipped.len(), 4, "{:?}", report.skipped);
    let file = read_pcs(dir.path());
    assert_eq!(
        file.pcs,
        vec![PcRecord {
            id: id_for(1),
            public: public_b64(1),
            registered_ms: 1_700_000_000_001,
            admitted: Vec::new(),
            remote_pairing: false,
        }]
    );
}

#[test]
fn import_names_the_mismatched_id_in_the_report() {
    let dir = tempfile::tempdir().unwrap();
    let report = import_go_auth(dir.path(), &go_auth_text()).unwrap();
    let mismatch = format!("{}: device ID does not match its public key", id_for(9));
    assert!(report.skipped.contains(&mismatch), "{:?}", report.skipped);
}

#[test]
fn import_twice_reports_the_pc_as_already_present() {
    let dir = tempfile::tempdir().unwrap();
    import_go_auth(dir.path(), &go_auth_text()).unwrap();
    let again = import_go_auth(dir.path(), &go_auth_text()).unwrap();
    assert_eq!(again.imported, 0);
    assert_eq!(again.already_present, 1);
    assert_eq!(read_pcs(dir.path()).pcs.len(), 1);
}

#[test]
fn import_merges_into_existing_pcs_without_dropping_them() {
    let dir = tempfile::tempdir().unwrap();
    let existing = PcsFile {
        version: 1,
        pcs: vec![PcRecord {
            id: id_for(7),
            public: public_b64(7),
            registered_ms: 77,
            admitted: Vec::new(),
            remote_pairing: false,
        }],
    };
    store::save_pcs(dir.path(), &existing).unwrap();
    let report = import_go_auth(dir.path(), &go_auth_text()).unwrap();
    assert_eq!(report.imported, 1);
    let ids: Vec<String> = read_pcs(dir.path()).pcs.into_iter().map(|r| r.id).collect();
    assert_eq!(ids, vec![id_for(7), id_for(1)]);
}

#[test]
fn import_without_a_pcs_table_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let report = import_go_auth(dir.path(), r#"{"version":1,"sessions":{}}"#).unwrap();
    assert_eq!(report.imported, 0);
    assert!(!dir.path().join(PCS_FILE).exists());
}

#[test]
fn import_refuses_other_auth_versions() {
    let dir = tempfile::tempdir().unwrap();
    let err = import_go_auth(dir.path(), r#"{"version":2,"pcs":{}}"#).unwrap_err();
    assert!(matches!(err, ImportError::Version(2)), "{err}");
}

#[test]
fn import_refuses_invalid_json_without_echoing_it() {
    let dir = tempfile::tempdir().unwrap();
    let err = import_go_auth(dir.path(), r#"{"passwordHash": "hunter2-secret", "#).unwrap_err();
    assert!(matches!(err, ImportError::Parse(_)), "{err}");
    assert!(!err.to_string().contains("hunter2-secret"));
}

#[test]
fn import_into_a_corrupt_pcs_file_is_refused_and_leaves_it_alone() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join(PCS_FILE), "garbage").unwrap();
    let err = import_go_auth(dir.path(), &go_auth_text()).unwrap_err();
    assert!(
        matches!(err, ImportError::Store(StoreError::Corrupt { .. })),
        "{err}"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join(PCS_FILE)).unwrap(),
        "garbage"
    );
}

#[test]
fn import_reads_the_auth_file_from_disk() {
    let dir = tempfile::tempdir().unwrap();
    let auth_path = dir.path().join("auth.json");
    fs::write(&auth_path, go_auth_text()).unwrap();
    let state = dir.path().join("state");
    let report = import_go_auth_file(&state, &auth_path).unwrap();
    assert_eq!(report.imported, 1);
    assert!(state.join(PCS_FILE).exists());
}

#[test]
fn state_records_survive_a_save_and_reload_through_import() {
    let dir = tempfile::tempdir().unwrap();
    import_go_auth(dir.path(), &go_auth_text()).unwrap();
    let before = read_pcs(dir.path());
    store::save_pcs(dir.path(), &before).unwrap();
    assert_eq!(read_pcs(dir.path()), before);
}

#[test]
fn init_output_loads_as_a_config_with_its_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("relay.json");
    write_example_config(&path).unwrap();
    let cfg = load_config(&path).expect("example config loads");
    assert_eq!(cfg.token.expose().len(), 43);
    assert_ne!(cfg.token.expose(), cfg.admin_token.expose());
}

#[test]
fn load_config_names_a_bad_field_from_disk() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("relay.json");
    fs::write(
        &path,
        r#"{"public_origin":"https://relay.example.com","admin_token":"a"}"#,
    )
    .unwrap();
    let message = load_config(&path).unwrap_err().to_string();
    assert!(message.starts_with("token:"), "{message}");
}
