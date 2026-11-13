//! fixtures/input-contract.json "pointerKeyTextPackets": the client's runtime packet encoders
//! against the shared vectors. Release-all and text have no runtime encoder (test-only), so
//! their vectors are skipped here.

use bubbleplay_client::input::protocol::{absolute, buttons, buttons_at, cursor_sync, key, motion};
use serde_json::Value;

const FIXTURE: &str = include_str!("../../fixtures/input-contract.json");

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn field(decoded: &Value, name: &str) -> i64 {
    decoded[name]
        .as_i64()
        .unwrap_or_else(|| panic!("{name} missing"))
}

fn encode(decoded: &Value) -> Option<String> {
    let kind = decoded["kind"]
        .as_str()
        .unwrap_or_else(|| panic!("kind missing"));
    let bytes = match kind {
        "key" => key(
            field(decoded, "vk") as u16,
            decoded["up"]
                .as_bool()
                .unwrap_or_else(|| panic!("up missing")),
        )
        .to_vec(),
        "buttons" => buttons(field(decoded, "mask") as u8).to_vec(),
        "motion" => motion(
            field(decoded, "seq") as u16,
            field(decoded, "dx") as i16,
            field(decoded, "dy") as i16,
            field(decoded, "wheelX") as i16,
            field(decoded, "wheelY") as i16,
        )
        .to_vec(),
        "absolute" => absolute(
            field(decoded, "seq") as u16,
            field(decoded, "x") as u16,
            field(decoded, "y") as u16,
        )
        .to_vec(),
        "buttonsAt" => buttons_at(
            field(decoded, "seq") as u16,
            field(decoded, "x") as u16,
            field(decoded, "y") as u16,
            field(decoded, "mask") as u8,
        )
        .to_vec(),
        "cursorSync" => cursor_sync(field(decoded, "seq") as u16).to_vec(),
        "releaseAll" | "text" => return None,
        other => panic!("unknown kind {other}"),
    };
    Some(hex(&bytes))
}

#[test]
fn client_encoders_produce_the_contract_bytes() {
    let all: Value = serde_json::from_str(FIXTURE).expect("fixture parses");
    let vectors = all["pointerKeyTextPackets"]["vectors"]
        .as_array()
        .expect("vectors");
    let mut checked = 0;
    for vector in vectors {
        if let Some(encoded) = encode(&vector["decoded"]) {
            assert_eq!(
                encoded,
                vector["hex"].as_str().expect("hex"),
                "{}",
                vector["name"]
            );
            checked += 1;
        }
    }
    assert!(checked >= 6, "only {checked} vectors reached an encoder");
}
