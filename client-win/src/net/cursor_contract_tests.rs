//! Cursor-shape packets against `fixtures/cursor-types.json`: every listed id is kept as sent,
//! anything past the table falls back to the arrow.

use serde_json::Value;

use super::input::InputLink;

#[test]
fn cursor_packets_follow_the_fixture_table() {
    let fixture: Value =
        serde_json::from_str(include_str!("../../../fixtures/cursor-types.json")).unwrap();
    let packet_type = fixture["packetType"].as_u64().unwrap() as u8;
    let fallback = fixture["fallback"].as_u64().unwrap() as u8;
    let ids: Vec<u8> = fixture["types"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["id"].as_u64().unwrap() as u8)
        .collect();
    assert_eq!(ids, (0..ids.len() as u8).collect::<Vec<_>>());

    let link = InputLink::default();
    for id in &ids {
        assert!(link.receive(&[packet_type, *id]), "id {id}");
        assert_eq!(link.snapshot().cursor_kind, *id, "id {id}");
    }
    let past_table = ids.len() as u8;
    for id in [past_table, 255] {
        link.receive(&[packet_type, id]);
        assert_eq!(link.snapshot().cursor_kind, fallback, "id {id}");
    }
}
