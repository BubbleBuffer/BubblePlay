use bubbleplay_host::wss_media::{shard_access_unit, AccessUnitMeta, MediaCodec, WssTopology};

#[test]
fn shared_wss_contract() {
    let contract: serde_json::Value =
        serde_json::from_str(include_str!("../../fixtures/wss-contract.json")).unwrap();
    for case in contract["topologies"].as_array().unwrap() {
        assert_eq!(
            WssTopology::new(
                case["laneCount"].as_u64().unwrap() as usize,
                case["dataShards"].as_u64().unwrap() as usize,
                case["parityShards"].as_u64().unwrap() as usize
            )
            .is_ok(),
            case["accepted"].as_bool().unwrap(),
            "{case}"
        );
    }
    let frame = &contract["v2"];
    let data: Vec<u8> = frame["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap() as u8)
        .collect();
    let packets = shard_access_unit(
        AccessUnitMeta {
            codec: MediaCodec::H264,
            generation: frame["generation"].as_u64().unwrap() as u32,
            gop: frame["gop"].as_u64().unwrap() as u32,
            sequence: frame["sequence"].as_u64().unwrap() as u32,
            timestamp90k: frame["timestamp90k"].as_u64().unwrap() as u32,
            key: true,
            lab_roi: None,
        },
        &data,
    )
    .unwrap();
    let hex: Vec<String> = packets
        .iter()
        .map(|p| p.iter().map(|b| format!("{b:02x}")).collect())
        .collect();
    assert_eq!(serde_json::to_value(hex).unwrap(), frame["packets"]);
}
