//! fixtures/input-contract.json "pointerKeyTextPackets": the host's input decoder against the
//! shared vectors.

use serde_json::Value;

use crate::input::{decode, Command};

const FIXTURE: &str = include_str!("../../fixtures/input-contract.json");

fn contract() -> Value {
    let all: Value = serde_json::from_str(FIXTURE).expect("fixture parses");
    all["pointerKeyTextPackets"].clone()
}

fn bytes(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).expect("hex byte"))
        .collect()
}

fn field(decoded: &Value, key: &str) -> i64 {
    decoded[key]
        .as_i64()
        .unwrap_or_else(|| panic!("{key} missing"))
}

fn expected(decoded: &Value) -> Command {
    let kind = decoded["kind"].as_str().expect("kind");
    match kind {
        "key" => Command::Key(
            field(decoded, "vk") as u16,
            decoded["up"].as_bool().expect("up"),
        ),
        "buttons" => Command::Buttons(field(decoded, "mask") as u8),
        "releaseAll" => Command::ReleaseAll,
        "motion" => Command::Motion(
            field(decoded, "seq") as u16,
            field(decoded, "dx") as i16,
            field(decoded, "dy") as i16,
            field(decoded, "wheelX") as i16,
            field(decoded, "wheelY") as i16,
        ),
        "absolute" => Command::Absolute(
            field(decoded, "seq") as u16,
            field(decoded, "x") as u16,
            field(decoded, "y") as u16,
        ),
        "buttonsAt" => Command::ButtonsAt(
            field(decoded, "seq") as u16,
            field(decoded, "x") as u16,
            field(decoded, "y") as u16,
            field(decoded, "mask") as u8,
        ),
        "cursorSync" => Command::CursorSync(field(decoded, "seq") as u16),
        "text" => Command::Text(decoded["text"].as_str().expect("text").to_owned()),
        other => panic!("unknown kind {other}"),
    }
}

#[test]
fn host_decodes_the_contract_vectors() {
    let contract = contract();
    for vector in contract["vectors"].as_array().expect("vectors") {
        let name = vector["name"].as_str().expect("name");
        let data = bytes(vector["hex"].as_str().expect("hex"));
        assert_eq!(decode(&data), Some(expected(&vector["decoded"])), "{name}");
    }
}

#[test]
fn host_rejects_malformed_packets_and_oversize_text() {
    let contract = contract();
    for hex in contract["malformedHex"].as_array().expect("malformedHex") {
        let hex = hex.as_str().expect("hex");
        assert_eq!(decode(&bytes(hex)), None, "{hex:?}");
    }
    let max = contract["textMaxBytes"].as_u64().expect("textMaxBytes") as usize;
    let text = |len: usize| [&[0x0b][..], vec![b'x'; len].as_slice()].concat();
    assert!(decode(&text(max)).is_some());
    assert_eq!(decode(&text(max + 1)), None);
}
