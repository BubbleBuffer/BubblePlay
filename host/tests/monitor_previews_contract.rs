//! fixtures/monitor-previews-contract.json: the host's sender side. Its limits, capability name
//! and message shape equal the fixture's, and a largest-allowed picture fits the message cap.

use base64::Engine as _;
use bubbleplay_host::monitor_previews::{
    message, CAPABILITY, MAX_JPEG_BYTES, MAX_WIDTH, MESSAGE_TYPE, MIN_INTERVAL, WIRE_VERSION,
};
use serde_json::Value;

const FIXTURE: &str = include_str!("../../fixtures/monitor-previews-contract.json");

#[test]
fn host_sender_matches_the_monitor_previews_contract() {
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    let limits = &fixture["limits"];
    assert_eq!(fixture["capability"], CAPABILITY);
    assert_eq!(fixture["version"], WIRE_VERSION);
    assert_eq!(limits["maxJpegBytes"], MAX_JPEG_BYTES);
    assert_eq!(limits["maxDimension"], MAX_WIDTH);
    assert_eq!(
        limits["minIntervalMsPerMonitor"],
        MIN_INTERVAL.as_millis() as u64
    );

    let sample = &fixture["messages"][0]["json"];
    assert_eq!(sample["type"], MESSAGE_TYPE);
    let jpeg = base64::engine::general_purpose::STANDARD
        .decode(sample["jpeg"].as_str().unwrap())
        .unwrap();
    let built = message(
        sample["monitor"].as_str().unwrap(),
        sample["seq"].as_u64().unwrap(),
        sample["w"].as_u64().unwrap() as u32,
        sample["h"].as_u64().unwrap() as u32,
        &jpeg,
    );
    assert_eq!(&built, sample);

    let largest = message(
        "m",
        u64::MAX,
        MAX_WIDTH,
        MAX_WIDTH,
        &vec![0; MAX_JPEG_BYTES],
    );
    let cap = limits["maxMessageJsonBytes"].as_u64().unwrap();
    assert!(largest.to_string().len() as u64 <= cap);
}
