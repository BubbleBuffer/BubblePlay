//! The names the host library shares with `setup-tray.ps1` come from `fixtures/install-names-contract.json`.
//! The tray's own names are checked in `host/src/tray/platform.rs`, the scripts in
//! `tools/tests/install-names.test.mjs`.

use bubbleplay_host::admin_agent::client::AGENT_EXE;
use bubbleplay_host::admin_mode::REGISTRY_KEY;
use bubbleplay_host::secure_helper::trust::HELPER_EXE;

fn fixture_value(name: &str) -> String {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("../../fixtures/install-names-contract.json"))
            .expect("fixture parses");
    fixture["values"][name]["value"]
        .as_str()
        .unwrap_or_else(|| panic!("fixture lacks {name}"))
        .to_owned()
}

#[test]
fn library_names_match_the_install_fixture() {
    assert_eq!(AGENT_EXE, fixture_value("agentExe"));
    assert_eq!(HELPER_EXE, fixture_value("helperExe"));
    assert_eq!(REGISTRY_KEY, fixture_value("hklmKey"));
}
