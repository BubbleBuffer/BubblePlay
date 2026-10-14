//! `--input-record`: every binary message received on `/ws` is written as one
//! JSON line, flushed on write, with the epoch, bytes, sequence (when present),
//! validity and the held keys and buttons after it. Packet bytes come from the
//! shared contract in `fixtures/input-contract.json`.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use common::*;
use futures_util::SinkExt;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

const WAIT: Duration = Duration::from_secs(2);
const FIXTURE: &str = include_str!("../../../../fixtures/input-contract.json");

/// Records go to cargo's per-target temp directory, which is on the target
/// junction (F:), never on C:.
fn record_path(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(&dir).expect("create the test temp dir");
    dir.join(name)
}

async fn send_binary(ws: &mut Ws, bytes: &[u8]) {
    ws.send(Message::Binary(bytes.to_vec().into()))
        .await
        .expect("send binary");
}

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("fixture is JSON")
}

/// The fixture's byte lists, as they are on the wire.
fn fixture_bytes(list: &str) -> Vec<Vec<u8>> {
    fixture()[list]
        .as_array()
        .expect("array")
        .iter()
        .map(|entry| {
            let raw = match entry.get("bytes") {
                Some(bytes) => bytes,
                None => entry,
            };
            raw.as_array()
                .expect("byte list")
                .iter()
                .map(|b| u8::try_from(b.as_u64().expect("byte")).expect("u8"))
                .collect::<Vec<u8>>()
        })
        .collect()
}

#[tokio::test]
async fn every_binary_message_on_ws_is_recorded_with_its_held_state() {
    let path = record_path("every-binary-message.jsonl");
    let harness = spawn_recording(
        synthetic(60, 30),
        Duration::from_millis(900),
        Duration::from_millis(100),
        &path,
    )
    .await;
    let (mut signal, epoch) = signal_hello(harness.addr).await;

    let packets: Vec<Vec<u8>> = vec![
        vec![13, 0, 29, 0, 0, 0], // scan 29 down, no extended flag
        vec![0, 0x13, 0],         // VK 19 down
        vec![4, 5],               // buttons: left + middle
        vec![7, 0xE8, 0x03, 0, 0, 0, 0, 0, 0, 0, 0], // motion v2, seq 1000
        vec![10, 0xE8, 0x03],     // cursor sync, seq 1000
        vec![13, 1, 29, 0, 0, 0], // scan 29 up
        vec![6],                  // release all
    ];
    for packet in &packets {
        send_binary(&mut signal, packet).await;
    }
    let snap = harness.finish().await;
    assert_eq!(
        snap.counters.rejected_control, 0,
        "valid input is not rejected"
    );

    let records = read_records(&path);
    assert_eq!(
        records.len(),
        packets.len(),
        "one record per binary message"
    );
    assert_eq!(
        records[0]["decoded"],
        serde_json::json!({"scan":29,"vk":0,"extended":false,"up":false})
    );
    assert_eq!(
        records[3]["decoded"],
        serde_json::json!({"seq":1000,"dx":0,"dy":0,"wheel_x":0,"wheel_y":0})
    );
    let raw = std::fs::read_to_string(&path).unwrap();
    let lifecycle: Vec<serde_json::Value> = raw
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .filter(|row: &serde_json::Value| row["event"] == "disconnect")
        .collect();
    assert!(lifecycle
        .iter()
        .all(|row| row.get("type").is_none()
            && row["held_after"]["keys"].as_array().unwrap().is_empty()));

    let kinds = [
        "scan-key",
        "key-down",
        "buttons",
        "motion-v2",
        "cursor-sync",
        "scan-key",
        "release-all",
    ];
    let held = [
        json!({"keys": [{"scan": 29, "extended": false}], "buttons": 0}),
        json!({"keys": [{"vk": 19}, {"scan": 29, "extended": false}], "buttons": 0}),
        json!({"keys": [{"vk": 19}, {"scan": 29, "extended": false}], "buttons": 5}),
        json!({"keys": [{"vk": 19}, {"scan": 29, "extended": false}], "buttons": 5}),
        json!({"keys": [{"vk": 19}, {"scan": 29, "extended": false}], "buttons": 5}),
        json!({"keys": [{"vk": 19}], "buttons": 5}),
        json!({"keys": [], "buttons": 0}),
    ];
    let types: [u8; 7] = [13, 0, 4, 7, 10, 13, 6];
    let mut last_ms = 0.0_f64;
    for (i, record) in records.iter().enumerate() {
        assert_eq!(record["index"], json!(i), "{record}");
        assert_eq!(record["epoch"], json!(epoch), "{record}");
        assert_eq!(record["bytes"], json!(packets[i]), "{record}");
        assert_eq!(record["len"], json!(packets[i].len()), "{record}");
        assert_eq!(record["type"], json!(types[i]), "{record}");
        assert_eq!(record["kind"], json!(kinds[i]), "{record}");
        assert_eq!(record["valid"], json!(true), "{record}");
        assert!(record["refused"].is_null(), "{record}");
        assert_eq!(record["held"], held[i], "held after record {i}: {record}");
        assert!(
            record["received_unix_ms"].as_u64().unwrap_or(0) > 0,
            "{record}"
        );
        let ms = record["received_ms"].as_f64().expect("received_ms");
        assert!(ms >= last_ms, "receipt times never go backwards: {record}");
        last_ms = ms;
    }
    // Only the motion and cursor-sync packets carry a sequence in this run.
    for (i, record) in records.iter().enumerate() {
        match i {
            3 | 4 => assert_eq!(record["seq"], json!(1000), "{record}"),
            _ => assert!(
                record.get("seq").is_none(),
                "no sequence for record {i}: {record}"
            ),
        }
    }
    // Release-all left nothing held.
    assert_eq!(
        records.last().unwrap()["held"],
        json!({"keys": [], "buttons": 0})
    );
}

#[tokio::test]
async fn refused_packets_are_recorded_as_refused_and_change_nothing() {
    let path = record_path("refused-packets.jsonl");
    let harness = spawn_recording(
        synthetic(60, 30),
        Duration::from_millis(900),
        Duration::from_millis(100),
        &path,
    )
    .await;
    let (mut signal, _epoch) = signal_hello(harness.addr).await;

    let mut packets: Vec<Vec<u8>> = vec![vec![0, 0x13, 0]];
    packets.extend(fixture_bytes("invalidPackets"));
    packets.push(vec![11]); // empty text
    packets.push(vec![11, 0xFF]); // not UTF-8
    packets.push(vec![129, 1]); // host-to-browser kind, refused inbound
    packets.push(vec![128, 0, 0, 0, 0, 0, 0, 0]); // host-to-browser state, refused inbound
    packets.push(vec![7, 5, 0]); // motion with a short body: refused, sequence shown
    packets.push(vec![0]); // key down with no key
    for packet in &packets {
        send_binary(&mut signal, packet).await;
    }
    let snap = harness.finish().await;
    assert_eq!(
        snap.counters.rejected_control as usize,
        packets.len() - 1,
        "only the malformed packets count as rejected control"
    );

    let records = read_records(&path);
    assert_eq!(records.len(), packets.len());
    let held_after_valid = json!({"keys": [{"vk": 19}], "buttons": 0});
    for (i, record) in records.iter().enumerate() {
        assert_eq!(record["bytes"], json!(packets[i]), "{record}");
        if i == 0 {
            assert_eq!(record["valid"], json!(true), "{record}");
            continue;
        }
        assert_eq!(record["valid"], json!(false), "{record}");
        assert_eq!(record["refused"], json!("malformed"), "{record}");
        assert_eq!(
            record["held"], held_after_valid,
            "refused packets change nothing: {record}"
        );
    }
    // The refused motion packet still shows the sequence bytes it carries.
    let motion = &records[records.len() - 2];
    assert_eq!(motion["seq"], json!(5), "{motion}");
    assert!(records.last().unwrap().get("seq").is_none());
}

#[tokio::test]
async fn fixture_packets_are_recorded_as_valid() {
    let path = record_path("fixture-packets.jsonl");
    let harness = spawn_recording(
        synthetic(60, 30),
        Duration::from_millis(900),
        Duration::from_millis(100),
        &path,
    )
    .await;
    let (mut signal, _epoch) = signal_hello(harness.addr).await;
    let packets = fixture_bytes("packets");
    assert!(packets.len() >= 5, "the fixture lists its valid packets");
    for packet in &packets {
        send_binary(&mut signal, packet).await;
    }
    harness.finish().await;

    let records = read_records(&path);
    assert_eq!(records.len(), packets.len());
    for (record, packet) in records.iter().zip(&packets) {
        assert_eq!(record["bytes"], json!(packet), "{record}");
        assert_eq!(record["valid"], json!(true), "{record}");
    }
    // Arrow-left down (scan 75, extended) is held from the first record. The
    // AltGr up for scan 56 releases nothing, since it was never pressed. Pause
    // (VK 19) and Shift-left (scan 42) are held by record 3.
    assert_eq!(
        records[0]["held"]["keys"],
        json!([{"scan": 75, "extended": true}])
    );
    assert_eq!(
        records[1]["held"]["keys"],
        json!([{"scan": 75, "extended": true}])
    );
    assert_eq!(
        records[3]["held"]["keys"],
        json!([
            {"vk": 19},
            {"scan": 42, "extended": false},
            {"scan": 75, "extended": true}
        ])
    );
    assert_eq!(records[0]["kind"], json!("scan-key"));
    assert_eq!(records[4]["kind"], json!("clipboard"));
}

#[tokio::test]
async fn records_are_flushed_while_the_run_is_live() {
    let path = record_path("flushed-live.jsonl");
    let harness = spawn_recording(
        synthetic(60, 30),
        Duration::from_millis(1500),
        Duration::from_millis(100),
        &path,
    )
    .await;
    let (mut signal, _epoch) = signal_hello(harness.addr).await;
    send_binary(&mut signal, &[6]).await;

    // The record is on disk, as a complete line, before the run ends.
    let deadline = std::time::Instant::now() + WAIT;
    let mut seen = false;
    while std::time::Instant::now() < deadline {
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        if text.ends_with('\n') {
            seen = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(seen, "the record was flushed while the run was still live");
    harness.finish().await;
    assert_eq!(read_records(&path).len(), 1);
}

#[tokio::test]
async fn media_socket_binary_is_not_recorded() {
    let path = record_path("media-not-recorded.jsonl");
    let harness = spawn_recording(
        synthetic(60, 30),
        Duration::from_millis(1500),
        Duration::from_millis(100),
        &path,
    )
    .await;
    let (_signal, mut media, _epoch) = streaming_session(harness.addr, "h264").await;
    send_binary(&mut media, &[6]).await;
    let snap = harness.finish().await;
    assert!(
        snap.counters.rejected_control >= 1,
        "media binary is still refused"
    );
    assert!(read_records(&path).is_empty(), "only /ws input is recorded");
}

#[tokio::test]
async fn a_record_file_that_cannot_be_created_fails_the_run() {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("no-such-directory")
        .join("input.jsonl");
    let harness = spawn_recording(
        synthetic(60, 30),
        Duration::from_millis(500),
        Duration::from_millis(100),
        &path,
    )
    .await;
    let result = harness.run.await.expect("server task");
    assert!(result.is_err(), "the run must not start without its record");
}
