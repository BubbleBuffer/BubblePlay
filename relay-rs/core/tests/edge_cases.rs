//! Edge cases per validator: just at and just over each size limit, wrong
//! JSON types, and extra keys where the key set is exact.

use bp_relay_core::{
    FILE_LIMIT, HELLO_LIMIT, Invalid, MEDIA_CONTROL_MAX, MEDIA_HEADER_BYTES, MEDIA_LIMIT,
    PENDING_LIMIT, QUEUE_BYTE_LIMIT, QUEUE_FRAMES, SIGNAL_LIMIT, Side,
    SignalKind, validate_file_binary, validate_media_binary, validate_media_text,
    validate_signal_text,
};

const EPOCH: &str = "0123456789abcdef0123456789abcdef";

fn host(text: &str) -> bool {
    validate_signal_text(Side::Host, text, EPOCH).is_ok()
}

fn viewer(text: &str) -> bool {
    validate_signal_text(Side::Viewer, text, EPOCH).is_ok()
}

/// base, padded with spaces (trailing JSON whitespace) to exactly len bytes.
fn padded(base: &str, len: usize) -> String {
    assert!(base.len() <= len, "{} > {len}", base.len());
    format!("{base}{}", " ".repeat(len - base.len()))
}

#[test]
fn limits_match_relay_server_go() {
    assert_eq!(SIGNAL_LIMIT, 512 * 1024);
    assert_eq!(HELLO_LIMIT, 16384);
    assert_eq!(MEDIA_HEADER_BYTES, 28);
    assert_eq!(MEDIA_LIMIT, 8 * 1024 * 1024 + 28);
    assert_eq!(FILE_LIMIT, 64 * 1024 + 16);
    assert_eq!(MEDIA_CONTROL_MAX, 512);
    assert_eq!(QUEUE_FRAMES, 80);
    assert_eq!(QUEUE_BYTE_LIMIT, 16 * 1024 * 1024);
    assert_eq!(PENDING_LIMIT, 64);
}

#[test]
fn signal_text_at_the_read_limit() {
    let base = r#"{"type":"bye"}"#;
    assert!(host(&padded(base, SIGNAL_LIMIT)));
    assert!(!host(&padded(base, SIGNAL_LIMIT + 1)));
}

#[test]
fn media_control_on_the_media_socket_at_512_bytes() {
    let base = r#"{"type":"keyframe","reason":"x"}"#;
    assert!(validate_media_text(Side::Viewer, &padded(base, 512), EPOCH).is_ok());
    assert!(validate_media_text(Side::Viewer, &padded(base, 513), EPOCH).is_err());
    assert!(validate_media_text(Side::Viewer, &padded(base, MEDIA_CONTROL_MAX), EPOCH).is_ok());
}

#[test]
fn media_binary_bounds() {
    assert!(validate_media_binary(Side::Host, &zeros(MEDIA_HEADER_BYTES)).is_err());
    assert!(validate_media_binary(Side::Host, &zeros(MEDIA_HEADER_BYTES + 1)).is_ok());
    assert!(validate_media_binary(Side::Host, &zeros(MEDIA_LIMIT)).is_ok());
    assert!(validate_media_binary(Side::Host, &zeros(MEDIA_LIMIT + 1)).is_err());
    assert!(validate_media_binary(Side::Viewer, &zeros(64)).is_err());
}

#[test]
fn signal_text_rejects_non_objects() {
    for raw in ["", "null", "[]", "5", "\"x\"", "{", "not json"] {
        assert!(!host(raw), "{raw}");
        assert!(!viewer(raw), "{raw}");
        assert!(
            validate_media_text(Side::Viewer, raw, EPOCH).is_err(),
            "{raw}"
        );
    }
    assert!(!host(r#"{"type":"approval-decision"}"#));
    assert!(!host(r#"{"type":"nope"}"#));
    assert!(!host("{}"));
}

#[test]
fn ping_is_accepted_from_both_sides_and_never_buffered() {
    for text in [r#"{"type":"ping"}"#, r#"{"type":"ping","atMs":1}"#] {
        let message = validate_signal_text(Side::Host, text, EPOCH).expect("ping");
        assert_eq!(message.kind(), SignalKind::Ping);
        assert!(message.consumed_by_relay());
        assert!(!message.bufferable());
        assert!(viewer(text));
    }
}

/// A BPF1 packet: header, transferId, seq, payload.
fn packet(kind: u8, transfer: u32, seq: u32, payload: &[u8]) -> Vec<u8> {
    let mut bytes = vec![b'B', b'P', b'F', b'1', kind, 0, 0, 0];
    bytes.extend_from_slice(&transfer.to_le_bytes());
    bytes.extend_from_slice(&seq.to_le_bytes());
    bytes.extend_from_slice(payload);
    bytes
}

fn meta(payload: &str) -> Vec<u8> {
    packet(1, 7, 0, payload.as_bytes())
}

#[test]
fn file_binary_header_rules() {
    let good = packet(2, 0, 0, &[]);
    assert_eq!(good.len(), 16);
    assert!(validate_file_binary(Side::Viewer, &good).is_ok());
    assert!(validate_file_binary(Side::Viewer, &good[..15]).is_err());
    assert!(validate_file_binary(Side::Viewer, &packet(0, 0, 0, &[])).is_err());
    assert!(validate_file_binary(Side::Viewer, &packet(9, 0, 0, &[])).is_err());
    let mut bad_magic = good.clone();
    bad_magic[0] = b'X';
    assert!(validate_file_binary(Side::Viewer, &bad_magic).is_err());
    for offset in [5, 6, 7] {
        let mut bad = good.clone();
        bad[offset] = 1;
        assert!(
            validate_file_binary(Side::Viewer, &bad).is_err(),
            "offset {offset}"
        );
    }
    // Only DATA carries a sequence number.
    assert!(validate_file_binary(Side::Viewer, &packet(2, 7, 1, &[])).is_err());
    assert!(validate_file_binary(Side::Viewer, &packet(3, 7, 1, b"x")).is_ok());
}

#[test]
fn file_binary_accept_and_data_payloads() {
    assert!(validate_file_binary(Side::Viewer, &packet(2, 7, 0, b"x")).is_err());
    assert!(validate_file_binary(Side::Viewer, &packet(3, 7, 0, b"")).is_err());
    let bytes = packet(3, 7, 0, b"hello");
    let packet = validate_file_binary(Side::Viewer, &bytes).expect("data");
    assert_eq!(packet.payload, b"hello");
}

#[test]
fn file_binary_ack_payloads() {
    for size in [0, 7, 9, 16] {
        assert!(
            validate_file_binary(Side::Viewer, &packet(4, 7, 0, &zeros(size))).is_err(),
            "ack of {size} bytes"
        );
    }
    let at_limit = 9_007_199_254_740_991u64.to_le_bytes();
    assert!(validate_file_binary(Side::Viewer, &packet(4, 7, 0, &at_limit)).is_ok());
    let over = 9_007_199_254_740_992u64.to_le_bytes();
    assert!(validate_file_binary(Side::Viewer, &packet(4, 7, 0, &over)).is_err());
    let max = u64::MAX.to_le_bytes();
    assert!(validate_file_binary(Side::Viewer, &packet(4, 7, 0, &max)).is_err());
}

#[test]
fn file_binary_done_payloads() {
    for size in [0, 31, 33] {
        assert!(
            validate_file_binary(Side::Viewer, &packet(5, 7, 0, &zeros(size))).is_err(),
            "done of {size} bytes"
        );
    }
    assert!(validate_file_binary(Side::Viewer, &packet(5, 7, 0, &[0; 32])).is_ok());
}

#[test]
fn file_binary_control_payload_bounds() {
    // The payload limit is 4096 bytes (trailing JSON whitespace counts); the
    // reason itself is bounded at 256 bytes.
    let at_limit = padded(r#"{"reason":"abc"}"#, 4096);
    assert!(validate_file_binary(Side::Viewer, &packet(6, 7, 0, at_limit.as_bytes())).is_ok());
    let over = padded(r#"{"reason":"abc"}"#, 4097);
    assert!(validate_file_binary(Side::Viewer, &packet(6, 7, 0, over.as_bytes())).is_err());
    let long_reason = format!(r#"{{"reason":"{}"}}"#, "a".repeat(256));
    assert!(validate_file_binary(Side::Viewer, &packet(6, 7, 0, long_reason.as_bytes())).is_ok());
    let longer_reason = format!(r#"{{"reason":"{}"}}"#, "a".repeat(257));
    assert!(
        validate_file_binary(Side::Viewer, &packet(6, 7, 0, longer_reason.as_bytes())).is_err()
    );
    assert!(validate_file_binary(Side::Viewer, &packet(6, 7, 0, b"")).is_err());
}

#[test]
fn file_binary_rejects_invalid_utf8_and_non_objects() {
    assert!(validate_file_binary(Side::Viewer, &packet(6, 7, 0, &[0x7b, 0xff, 0x7d])).is_err());
    assert!(validate_file_binary(Side::Viewer, &packet(6, 7, 0, b"[]")).is_err());
    assert!(validate_file_binary(Side::Viewer, &packet(6, 7, 0, b"null")).is_err());
    assert!(validate_file_binary(Side::Viewer, &packet(6, 7, 0, b"{")).is_err());
}

#[test]
fn file_binary_request_only_from_the_viewer() {
    let request = packet(8, 9, 0, br#"{"offerId":4,"id":2}"#);
    assert!(validate_file_binary(Side::Viewer, &request).is_ok());
    assert!(validate_file_binary(Side::Host, &request).is_err());
    let partial = packet(8, 9, 0, br#"{"offerId":4}"#);
    assert!(validate_file_binary(Side::Viewer, &partial).is_err());
    let string_id = packet(8, 9, 0, br#"{"offerId":4,"id":"2"}"#);
    assert!(validate_file_binary(Side::Viewer, &string_id).is_err());
}

#[test]
fn file_binary_result_error_field() {
    assert!(validate_file_binary(Side::Viewer, &packet(7, 7, 0, br#"{"ok":true}"#)).is_ok());
    assert!(
        validate_file_binary(
            Side::Viewer,
            &packet(7, 7, 0, br#"{"ok":true,"error":null}"#)
        )
        .is_ok()
    );
    let long = format!(r#"{{"ok":false,"error":"{}"}}"#, "e".repeat(256));
    assert!(validate_file_binary(Side::Viewer, &packet(7, 7, 0, long.as_bytes())).is_ok());
    let too_long = format!(r#"{{"ok":false,"error":"{}"}}"#, "e".repeat(257));
    assert!(validate_file_binary(Side::Viewer, &packet(7, 7, 0, too_long.as_bytes())).is_err());
    assert!(validate_file_binary(Side::Viewer, &packet(7, 7, 0, br#"{"ok":1}"#)).is_err());
    assert!(validate_file_binary(Side::Viewer, &packet(7, 7, 0, br#"{"error":"x"}"#)).is_err());
}

#[test]
fn file_binary_cancel_reason_bound() {
    let at = format!(r#"{{"reason":"{}"}}"#, "a".repeat(256));
    assert!(validate_file_binary(Side::Viewer, &packet(6, 7, 0, at.as_bytes())).is_ok());
    let over = format!(r#"{{"reason":"{}"}}"#, "a".repeat(257));
    assert!(validate_file_binary(Side::Viewer, &packet(6, 7, 0, over.as_bytes())).is_err());
}

#[test]
fn file_binary_at_the_read_limit() {
    let payload = vec![0xab; 64 * 1024];
    let at = packet(3, 7, 0, &payload);
    assert_eq!(at.len(), FILE_LIMIT);
    assert!(validate_file_binary(Side::Viewer, &at).is_ok());
    let over = packet(3, 7, 0, &vec![0xab; 64 * 1024 + 1]);
    assert_eq!(over.len(), FILE_LIMIT + 1);
    assert!(validate_file_binary(Side::Viewer, &over).is_err());
}

#[test]
fn file_offer_metadata_fields() {
    let base = r#"{"name":"a","size":1,"mime":"","mtimeMs":0,"purpose":"save","batchId":1,"batchIndex":0,"batchCount":1}"#;
    assert!(validate_file_binary(Side::Viewer, &meta(base)).is_ok());
    // size at and over the 2 GiB ceiling.
    assert!(
        validate_file_binary(
            Side::Viewer,
            &meta(&base.replace(r#""size":1"#, r#""size":2147483648"#))
        )
        .is_ok()
    );
    assert!(
        validate_file_binary(
            Side::Viewer,
            &meta(&base.replace(r#""size":1"#, r#""size":2147483649"#))
        )
        .is_err()
    );
    // mtimeMs at and over the 2^53 ceiling.
    assert!(
        validate_file_binary(
            Side::Viewer,
            &meta(&base.replace(r#""mtimeMs":0"#, r#""mtimeMs":9007199254740991"#))
        )
        .is_ok()
    );
    assert!(
        validate_file_binary(
            Side::Viewer,
            &meta(&base.replace(r#""mtimeMs":0"#, r#""mtimeMs":9007199254740992"#))
        )
        .is_err()
    );
    // An empty name is not a name.
    assert!(
        validate_file_binary(
            Side::Viewer,
            &meta(&base.replace(r#""name":"a""#, r#""name":""#))
        )
        .is_err()
    );
    // batchIndex must be below batchCount.
    assert!(
        validate_file_binary(
            Side::Viewer,
            &meta(&base.replace(r#""batchIndex":0"#, r#""batchIndex":1"#))
        )
        .is_err()
    );
    let two = base
        .replace(r#""batchCount":1"#, r#""batchCount":2"#)
        .replace(r#""batchIndex":0"#, r#""batchIndex":1"#);
    assert!(validate_file_binary(Side::Viewer, &meta(&two)).is_ok());
}

#[test]
fn file_offer_drop_purpose_requires_a_valid_pointer() {
    let drop = r#"{"name":"a","size":1,"mime":"","mtimeMs":0,"purpose":"drop","batchId":1,"batchIndex":0,"batchCount":1,"drop":{"x":32767,"y":20000,"generation":1,"viewportRevision":0}}"#;
    assert!(validate_file_binary(Side::Viewer, &meta(drop)).is_ok());
    assert!(
        validate_file_binary(
            Side::Viewer,
            &meta(&drop.replace(r#""x":32767"#, r#""x":65536"#))
        )
        .is_err()
    );
    assert!(
        validate_file_binary(
            Side::Viewer,
            &meta(&drop.replace(r#""generation":1"#, r#""generation":0"#))
        )
        .is_err()
    );
    // save/paste must not carry a drop pointer.
    let save = drop.replace(r#""purpose":"drop""#, r#""purpose":"paste""#);
    assert!(validate_file_binary(Side::Viewer, &meta(&save)).is_err());
    assert!(
        validate_file_binary(
            Side::Viewer,
            &meta(&drop.replace(r#""purpose":"drop""#, r#""purpose":"open""#))
        )
        .is_err()
    );
}

#[test]
fn media_ack_and_keyframe_key_sets() {
    let ack = r#"{"type":"ack","generation":1,"seq":0,"recvTsMs":12.3,"decodedSeq":0}"#;
    assert!(validate_media_text(Side::Viewer, ack, EPOCH).is_ok());
    assert!(
        validate_media_text(Side::Viewer, &ack.replace(r#","decodedSeq":0"#, ""), EPOCH).is_ok()
    );
    assert!(
        validate_media_text(
            Side::Viewer,
            &ack.replace(r#""decodedSeq":0"#, r#""decodedSeq":-1"#),
            EPOCH
        )
        .is_err()
    );
    assert!(
        validate_media_text(
            Side::Viewer,
            &ack.replace(r#"}"#, r#","lanes":[1]}"#),
            EPOCH
        )
        .is_err()
    );
    assert!(
        validate_media_text(
            Side::Viewer,
            &ack.replace(r#""seq":0"#, r#""seq":1.5"#),
            EPOCH
        )
        .is_err()
    );
    assert!(
        validate_media_text(
            Side::Viewer,
            &ack.replace(r#""recvTsMs":12.3"#, r#""recvTsMs":-1"#),
            EPOCH
        )
        .is_err()
    );
    assert!(
        validate_media_text(
            Side::Viewer,
            &ack.replace(r#""recvTsMs":12.3"#, r#""recvTsMs":"x""#),
            EPOCH
        )
        .is_err()
    );
    assert!(validate_media_text(Side::Host, ack, EPOCH).is_err());
    let keyframe = r#"{"type":"keyframe","reason":"decoder:lost"}"#;
    assert!(validate_media_text(Side::Viewer, keyframe, EPOCH).is_ok());
    assert!(
        validate_media_text(Side::Viewer, r#"{"type":"keyframe","reason":""}"#, EPOCH).is_err()
    );
    assert!(
        validate_media_text(
            Side::Viewer,
            r#"{"type":"keyframe","reason":"bad reason!"}"#,
            EPOCH
        )
        .is_err()
    );
    assert!(validate_media_text(Side::Viewer, r#"{"type":"keyframe"}"#, EPOCH).is_err());
    assert!(validate_media_text(Side::Viewer, r#"{"type":"viewport"}"#, EPOCH).is_err());
}

#[test]
fn binary_frames_close_past_the_read_limit_and_on_file_breaks() {
    assert_eq!(
        validate_media_binary(Side::Host, &zeros(MEDIA_LIMIT + 1)).unwrap_err(),
        Invalid::Close
    );
    assert_eq!(
        validate_media_binary(Side::Host, &zeros(MEDIA_HEADER_BYTES)).unwrap_err(),
        Invalid::Drop
    );
    assert_eq!(
        validate_media_binary(Side::Viewer, &zeros(64)).unwrap_err(),
        Invalid::Drop
    );
    assert_eq!(
        validate_file_binary(Side::Viewer, &[0; 15]).unwrap_err(),
        Invalid::Close
    );
    assert_eq!(
        validate_file_binary(Side::Viewer, &zeros(FILE_LIMIT + 1)).unwrap_err(),
        Invalid::Close
    );
}

#[test]
fn file_ack_payload_is_exactly_eight_bytes() {
    assert!(validate_file_binary(Side::Viewer, &packet(4, 7, 0, &[0; 7])).is_err());
    assert!(validate_file_binary(Side::Viewer, &packet(4, 7, 0, &[0; 9])).is_err());
    let max = 9_007_199_254_740_991u64.to_le_bytes();
    assert!(validate_file_binary(Side::Viewer, &packet(4, 7, 0, &max)).is_ok());
    let over = 9_007_199_254_740_992u64.to_le_bytes();
    assert!(validate_file_binary(Side::Viewer, &packet(4, 7, 0, &over)).is_err());
}

/// `len` zero bytes.
fn zeros(len: usize) -> Vec<u8> {
    vec![0; len]
}
