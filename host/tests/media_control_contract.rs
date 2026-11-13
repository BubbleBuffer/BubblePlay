//! fixtures/media-control-contract.json: the host accepts every viewer message as the variant
//! and phase or action the fixture names.

use bubbleplay_host::signaling::SignalMsg;
use serde_json::Value;

const FIXTURE: &str = include_str!("../../fixtures/media-control-contract.json");

#[test]
fn host_parses_every_viewer_message_in_the_fixture() {
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    let mut checked = 0;
    for message in fixture["clientMessages"].as_array().unwrap() {
        let name = message["name"].as_str().unwrap();
        let Some(expect) = message.get("hostParses") else {
            continue;
        };
        let parsed: SignalMsg = serde_json::from_value(message["json"].clone())
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        match (expect["variant"].as_str().unwrap(), parsed) {
            (
                "media-control",
                SignalMsg::MediaControl {
                    action,
                    epoch,
                    protocol,
                    ..
                },
            ) => {
                assert_eq!(Some(action.as_str()), expect["action"].as_str(), "{name}");
                assert_eq!(
                    epoch.as_deref(),
                    message["json"]["epoch"].as_str(),
                    "{name}"
                );
                assert_eq!(
                    protocol.map(u64::from),
                    message["json"]["protocol"].as_u64(),
                    "{name}"
                );
            }
            (
                "first-frame",
                SignalMsg::FirstFrame {
                    phase,
                    request_id,
                    generation,
                    ..
                },
            ) => {
                assert_eq!(Some(phase.as_str()), expect["phase"].as_str(), "{name}");
                assert_eq!(
                    Some(request_id.as_str()),
                    message["json"]["requestId"].as_str(),
                    "{name}"
                );
                assert_eq!(
                    generation.map(u64::from),
                    message["json"]["generation"].as_u64(),
                    "{name}"
                );
            }
            (variant, other) => panic!("{name}: expected {variant}, parsed {other:?}"),
        }
        checked += 1;
    }
    assert_eq!(checked, 4);
}
