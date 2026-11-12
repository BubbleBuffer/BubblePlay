//! The viewer's signaling builders against `fixtures/media-control-contract.json`: each builder,
//! fed the fixture's own values, must produce exactly the fixture's JSON.

use serde_json::Value;

use super::protocol::{
    first_frame_keyframe, first_frame_presented, first_frame_request, media_hello, start_message,
};
use super::Codec;

fn built(name: &str, fixture: &Value) -> String {
    let json = &fixture["json"];
    let text = |key: &str| json[key].as_str().unwrap_or_default().to_string();
    let codec = |wire: &str| match wire {
        "hevc" => Codec::Hevc,
        "h264" => Codec::H264,
        other => panic!("{name}: codec {other}"),
    };
    match name {
        "start" => start_message(&text("epoch"), codec(&text("codec"))),
        "first-frame-request" => first_frame_request(&text("requestId"), &text("epoch")),
        "first-frame-presented" => first_frame_presented(
            &text("requestId"),
            json["generation"].as_u64().unwrap() as u32,
        ),
        "first-frame-timeout-keyframe" => {
            first_frame_keyframe(&text("epoch"), codec(&text("codec")))
        }
        "media-hello" => media_hello(&text("ticket")),
        other => panic!("fixture message {other} has no client-win builder"),
    }
}

#[test]
fn builders_match_the_media_control_fixture() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../fixtures/media-control-contract.json"
    ))
    .unwrap();
    let mut checked = 0;
    for message in fixture["clientMessages"].as_array().unwrap() {
        let builders = message["builtBy"].as_array().unwrap();
        if !builders.iter().any(|v| v == "client-win") {
            continue;
        }
        let name = message["name"].as_str().unwrap();
        let sent: Value = serde_json::from_str(&built(name, message)).unwrap();
        assert_eq!(sent, message["json"], "{name}");
        checked += 1;
    }
    assert_eq!(checked, 5);
}
