//! Edge cases per validator: just at and just over each size limit, wrong
//! JSON types, and extra keys where the key set is exact.

use bp_relay_core::{
    FILE_LIMIT, FileText, HELLO_LIMIT, Invalid, MEDIA_CONTROL_MAX, MEDIA_HEADER_BYTES, MEDIA_LIMIT,
    PENDING_LIMIT, QUEUE_BYTE_LIMIT, QUEUE_FRAMES, SIGNAL_BINARY_MAX, SIGNAL_LIMIT, Side,
    SignalKind, validate_file_binary, validate_file_text, validate_media_binary,
    validate_media_text, validate_signal_binary, validate_signal_text,
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
    assert_eq!(SIGNAL_BINARY_MAX, 16384);
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
fn offer_sdp_at_the_256k_limit() {
    let at = format!(
        r#"{{"type":"offer","sdp":"{}"}}"#,
        "a".repeat(256 * 1024 - 25)
    );
    assert_eq!(at.len(), 256 * 1024);
    assert!(viewer(&at));
    let over = format!(
        r#"{{"type":"offer","sdp":"{}"}}"#,
        "a".repeat(256 * 1024 - 24)
    );
    assert_eq!(over.len(), 256 * 1024 + 1);
    assert!(!viewer(&over));
}

#[test]
fn ice_at_the_16k_limit() {
    let base = r#"{"type":"ice","candidate":"x"}"#;
    assert!(host(&padded(base, 16384)));
    assert!(viewer(&padded(base, 16384)));
    assert!(!host(&padded(base, 16385)));
}

#[test]
fn client_log_at_the_2k_limit() {
    let base = r#"{"type":"client-log","seq":1,"atMs":2,"level":"info","event":"e","detail":{}}"#;
    assert!(viewer(&padded(base, 2048)));
    assert!(!viewer(&padded(base, 2049)));
}

#[test]
fn quality_at_the_1k_limit() {
    let base = r#"{"type":"quality"}"#;
    assert!(host(&padded(base, 1024)));
    assert!(!host(&padded(base, 1025)));
}

#[test]
fn desktop_state_at_the_512_limit() {
    let base = r#"{"type":"desktop-state","state":"active","generation":1,"reason":""}"#;
    assert!(host(&padded(base, 512)));
    assert!(!host(&padded(base, 513)));
}

#[test]
fn input_capabilities_at_the_256_limit() {
    let base = r#"{"type":"input-capabilities","protocol":1,"scancode":true,"clipboard":false,"clipboardMaxBytes":0}"#;
    assert!(host(&padded(base, 256)));
    assert!(!host(&padded(base, 257)));
}

#[test]
fn media_control_on_the_signal_socket_at_the_2k_limit() {
    let base = r#"{"type":"media-control","action":"start"}"#;
    assert!(viewer(&padded(base, 2048)));
    assert!(!viewer(&padded(base, 2049)));
}

#[test]
fn media_control_on_the_media_socket_at_512_bytes() {
    let base = r#"{"type":"keyframe","reason":"x"}"#;
    assert!(validate_media_text(Side::Viewer, &padded(base, 512), EPOCH).is_ok());
    assert!(validate_media_text(Side::Viewer, &padded(base, 513), EPOCH).is_err());
    assert!(validate_media_text(Side::Viewer, &padded(base, MEDIA_CONTROL_MAX), EPOCH).is_ok());
}

#[test]
fn first_frame_at_the_1k_limit() {
    let base = r#"{"type":"first-frame","protocol":1,"phase":"request","requestId":"abcdefgh","transport":"wss"}"#;
    assert!(viewer(&padded(base, 1024)));
    assert!(!viewer(&padded(base, 1025)));
}

#[test]
fn media_view_at_the_64k_limit() {
    let base = r#"{"type":"media-view"}"#;
    assert!(host(&padded(base, 64 * 1024)));
    assert!(!host(&padded(base, 64 * 1024 + 1)));
}

#[test]
fn bench_status_at_the_16k_limit() {
    let base = r#"{"type":"bench-status","protocol":1}"#;
    assert!(host(&padded(base, 16384)));
    assert!(!host(&padded(base, 16385)));
}

#[test]
fn clipboard_at_its_own_limit() {
    let base = r#"{"type":"clipboard","protocol":1,"id":1,"index":0,"count":1,"text":""}"#;
    assert!(host(&padded(base, 32 * 1024 * 6 + 256)));
    assert!(!host(&padded(base, 32 * 1024 * 6 + 257)));
}

#[test]
fn signal_binary_bounds() {
    assert!(validate_signal_binary(&[]).is_err());
    assert!(validate_signal_binary(&[0]).is_ok());
    assert!(validate_signal_binary(&zeros(SIGNAL_BINARY_MAX)).is_ok());
    assert!(validate_signal_binary(&zeros(SIGNAL_BINARY_MAX + 1)).is_err());
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

fn kind_of(side: Side, text: &str) -> Option<SignalKind> {
    validate_signal_text(side, text, EPOCH)
        .ok()
        .map(|m| m.kind())
}

#[test]
fn bye_and_secure_action_key_sets() {
    assert!(!host(r#"{"type":"bye","atMs":1}"#));
    assert!(host(r#"{"type":"bye"}"#));
    assert!(viewer(r#"{"type":"secure-action","action":"deny"}"#));
    assert!(!host(r#"{"type":"secure-action","action":"deny"}"#));
    assert!(!viewer(r#"{"type":"secure-action","action":"allow"}"#));
    assert!(!viewer(r#"{"type":"secure-action","action":"deny","x":0}"#));
    assert!(!viewer(r#"{"type":"secure-action"}"#));
    assert!(host(
        r#"{"type":"secure-action-result","action":"deny","ok":true,"reason":""}"#
    ));
    assert!(!host(
        r#"{"type":"secure-action-result","action":"deny","ok":true}"#
    ));
    assert!(!host(
        r#"{"type":"secure-action-result","action":"deny","ok":1,"reason":""}"#
    ));
    assert!(!host(
        r#"{"type":"secure-action-result","action":"deny","ok":true,"reason":null}"#
    ));
    assert!(!host(
        r#"{"type":"secure-action-result","action":"deny","ok":true,"reason":"","x":1}"#
    ));
}

#[test]
fn input_capabilities_and_desktop_state_key_sets() {
    let capabilities = r#"{"type":"input-capabilities","protocol":1,"scancode":true,"clipboard":true,"clipboardMaxBytes":1048576}"#;
    assert_eq!(
        kind_of(Side::Host, capabilities),
        Some(SignalKind::InputCapabilities)
    );
    assert!(!viewer(capabilities));
    assert!(!host(
        &capabilities.replace(r#""scancode":true"#, r#""scancode":1"#)
    ));
    assert!(!host(&capabilities.replace(
        r#""clipboardMaxBytes":1048576"#,
        r#""clipboardMaxBytes":1048577"#
    )));
    assert!(!host(
        &capabilities.replace(r#""protocol":1"#, r#""protocol":2"#)
    ));
    assert!(!host(&capabilities.replace(r#"}"#, r#","extra":1}"#)));
    let state = r#"{"type":"desktop-state","state":"active","generation":1,"reason":""}"#;
    assert!(!host(&state.replace(r#"}"#, r#","extra":1}"#)));
    assert!(!host(&state.replace(r#","reason":"""#, "")));
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
fn host_capabilities_requires_the_full_form() {
    let full = r#"{"type":"host-capabilities","serviceMode":false,"coldBoot":false,"secureDesktop":false,"secureText":false}"#;
    assert_eq!(
        kind_of(Side::Host, full),
        Some(SignalKind::HostCapabilities)
    );
    assert!(!viewer(full));
    // The approval-only form is gone with browser approval.
    assert!(!host(
        r#"{"type":"host-capabilities","browserApproval":true}"#
    ));
    assert!(!host(
        r#"{"type":"host-capabilities","browserApproval":false}"#
    ));
    assert!(!host(r#"{"type":"host-capabilities"}"#));
    assert!(!host(
        &full.replace(r#""coldBoot":false"#, r#""coldBoot":0"#)
    ));
}

#[test]
fn host_capabilities_optional_flags() {
    let base = r#"{"type":"host-capabilities","serviceMode":false,"coldBoot":false,"secureDesktop":false,"secureText":false"#;
    let with = |extra: &str| format!("{base}{extra}}}");
    assert!(host(&with(r#","browserApproval":true"#)));
    assert!(!host(&with(r#","browserApproval":null"#)));
    assert!(host(&with(r#","fileTransfer":255"#)));
    assert!(!host(&with(r#","fileTransfer":256"#)));
    assert!(!host(&with(r#","fileTransfer":-1"#)));
    assert!(host(&with(r#","fileTransfer":1,"fileDrop":1"#)));
    assert!(!host(&with(r#","fileDrop":1"#)));
    assert!(!host(&with(r#","fileTransfer":2,"fileDrop":1"#)));
    assert!(host(&with(
        r#","fileTransfer":1,"fileDrop":1,"fileDropPreview":1"#
    )));
    assert!(!host(&with(r#","fileTransfer":1,"fileDropPreview":1"#)));
    assert!(host(&with(r#","followWindow":255"#)));
    assert!(!host(&with(r#","followWindow":256"#)));
    // serviceMode rules out the drop preview.
    let service = r#"{"type":"host-capabilities","serviceMode":true,"coldBoot":false,"secureDesktop":false,"secureText":false,"fileTransfer":1,"fileDrop":1,"fileDropPreview":1}"#;
    assert!(!host(service));
    let raw = with(r#","browserApproval":true"#);
    let message = validate_signal_text(Side::Host, &raw, EPOCH).expect("caps");
    assert!(message.browser_approval());
    assert!(message.cached_for_viewers());
    assert!(message.consumed_by_relay());
}

#[test]
fn file_offer_message_shapes() {
    let base = r#"{"type":"file-offer","protocol":1,"offerId":7,"items":[],"source":"clipboard"}"#;
    assert_eq!(kind_of(Side::Host, base), Some(SignalKind::FileOffer));
    assert!(!viewer(base));
    assert!(host(&base.replace(r#""items":[]"#, r#""items":null"#)));
    assert!(!host(&base.replace(r#","items":[]"#, "")));
    assert!(!host(
        &base.replace(r#""source":"clipboard""#, r#""source":"disk""#)
    ));
    assert!(!host(&base.replace(r#""protocol":1"#, r#""protocol":2"#)));
    assert!(!host(&base.replace(r#""offerId":7"#, r#""offerId":-1"#)));
    let items = (0..65)
        .map(|id| format!(r#"{{"id":{id},"name":"f","size":1}}"#))
        .collect::<Vec<_>>()
        .join(",");
    assert!(!host(
        &base.replace(r#""items":[]"#, &format!(r#""items":[{items}]"#))
    ));
    let items = (0..64)
        .map(|id| format!(r#"{{"id":{id},"name":"f","size":1}}"#))
        .collect::<Vec<_>>()
        .join(",");
    assert!(host(
        &base.replace(r#""items":[]"#, &format!(r#""items":[{items}]"#))
    ));
    assert!(!host(&base.replace(
        r#""items":[]"#,
        r#""items":[{"id":1,"name":"","size":1}]"#
    )));
    assert!(!host(&base.replace(
        r#""items":[]"#,
        r#""items":[{"id":1,"name":"f","size":2147483649}]"#
    )));
    assert!(!host(
        &base.replace(r#""items":[]"#, r#""items":[{"id":1,"name":"f"}]"#)
    ));
}

#[test]
fn file_drag_preview_shapes() {
    let update = format!(
        r#"{{"type":"file-drag-preview","protocol":1,"epoch":"{EPOCH}","dragId":1,"action":"update","drop":{{"x":1,"y":2,"generation":1,"viewportRevision":0}},"types":["text/plain"]}}"#
    );
    assert_eq!(
        kind_of(Side::Viewer, &update),
        Some(SignalKind::FileDragPreview)
    );
    assert!(!host(&update));
    assert!(!viewer(
        &update.replace(EPOCH, "ffffffffffffffffffffffffffffffff")
    ));
    assert!(!viewer(
        &update.replace(r#""action":"update""#, r#""action":"cancel""#)
    ));
    let cancel = format!(
        r#"{{"type":"file-drag-preview","protocol":1,"epoch":"{EPOCH}","dragId":1,"action":"cancel"}}"#
    );
    assert!(viewer(&cancel));
    assert!(!viewer(&update.replace(r#","types":["text/plain"]"#, "")));
    assert!(!viewer(&update.replace(r#"["text/plain"]"#, "[]")));
    assert!(!viewer(
        &update.replace(r#"["text/plain"]"#, r#"["bad type"]"#)
    ));
    assert!(!viewer(&update.replace(r#"["text/plain"]"#, r#"[1]"#)));
    assert!(!viewer(&update.replace(r#""dragId":1"#, r#""dragId":0"#)));
    assert!(!viewer(&update.replace(r#"}"#, r#","extra":1}"#)));
    let many = (0..65)
        .map(|index| format!(r#""t{index}""#))
        .collect::<Vec<_>>()
        .join(",");
    assert!(!viewer(
        &update.replace(r#"["text/plain"]"#, &format!("[{many}]"))
    ));
}

#[test]
fn first_frame_phases() {
    let request = format!(
        r#"{{"type":"first-frame","protocol":1,"phase":"request","requestId":"abcdefgh","transport":"wss","epoch":"{EPOCH}"}}"#
    );
    assert_eq!(
        kind_of(Side::Viewer, &request),
        Some(SignalKind::FirstFrame)
    );
    assert!(!host(&request));
    // epoch is optional on a request: without the key the request is valid.
    assert!(viewer(&request.replace(r#","epoch":""#, r#","epochX":""#)));
    assert!(viewer(&request.replace(r#","epoch":"#, r#","other":"#)));
    assert!(!viewer(
        &request.replace(EPOCH, "ffffffffffffffffffffffffffffffff")
    ));
    assert!(!viewer(
        &request.replace(r#""transport":"wss""#, r#""transport":"udp""#)
    ));
    assert!(!viewer(&request.replace(
        r#""requestId":"abcdefgh""#,
        r#""requestId":"1234567""#
    )));
    assert!(viewer(
        r#"{"type":"first-frame","protocol":1,"phase":"presented","requestId":"abcdefgh","generation":0}"#
    ));
    assert!(!viewer(
        r#"{"type":"first-frame","protocol":1,"phase":"presented","requestId":"abcdefgh"}"#
    ));
    assert!(host(
        r#"{"type":"first-frame","protocol":1,"phase":"admitted","requestId":"abcdefgh","generation":12}"#
    ));
    assert!(host(
        r#"{"type":"first-frame","protocol":1,"phase":"ready","requestId":"abcdefgh","generation":12}"#
    ));
    assert!(!viewer(
        r#"{"type":"first-frame","protocol":1,"phase":"admitted","requestId":"abcdefgh","generation":12}"#
    ));
    assert!(host(
        r#"{"type":"first-frame","protocol":1,"phase":"failed","requestId":"abcdefgh","generation":1,"reason":"x"}"#
    ));
    assert!(!host(
        r#"{"type":"first-frame","protocol":1,"phase":"failed","requestId":"abcdefgh","generation":1,"reason":""}"#
    ));
    assert!(!host(
        r#"{"type":"first-frame","protocol":1,"phase":"unknown","requestId":"abcdefgh","generation":1}"#
    ));
}

#[test]
fn media_control_actions() {
    for action in [
        "start", "stop", "viewport", "profile", "keyframe", "mode", "quality",
    ] {
        assert!(
            viewer(&format!(
                r#"{{"type":"media-control","action":"{action}"}}"#
            )),
            "{action}"
        );
    }
    let monitors =
        format!(r#"{{"type":"media-control","action":"monitors","protocol":4,"epoch":"{EPOCH}"}}"#);
    assert!(viewer(&monitors));
    assert!(!viewer(r#"{"type":"media-control","action":"monitors"}"#));
    assert!(!viewer(
        &monitors.replace(EPOCH, "ffffffffffffffffffffffffffffffff")
    ));
    assert!(!viewer(
        r#"{"type":"media-control","action":"monitor","monitor":""}"#
    ));
    assert!(viewer(
        r#"{"type":"media-control","action":"monitor","monitor":"primary"}"#
    ));
    let follow = format!(
        r#"{{"type":"media-control","action":"follow","protocol":4,"epoch":"{EPOCH}","follow":true}}"#
    );
    assert!(viewer(&follow));
    assert!(!viewer(
        &follow.replace(r#""follow":true"#, r#""follow":1"#)
    ));
    // A protocol 4 message with a stale epoch is dropped whatever the action.
    assert!(!viewer(
        r#"{"type":"media-control","action":"start","protocol":4,"epoch":"ffffffffffffffffffffffffffffffff"}"#
    ));
    assert!(!viewer(r#"{"type":"media-control","action":"nope"}"#));
    assert!(!viewer(r#"{"type":"media-control"}"#));
}

#[test]
fn clipboard_and_clipboard_image() {
    let clipboard = r#"{"type":"clipboard","protocol":1,"id":1,"index":0,"count":2,"text":"abc"}"#;
    assert_eq!(kind_of(Side::Host, clipboard), Some(SignalKind::Clipboard));
    assert!(!viewer(clipboard));
    assert!(!host(&clipboard.replace(r#""index":0"#, r#""index":2"#)));
    assert!(host(&clipboard.replace(r#""index":0"#, r#""index":1"#)));
    assert!(!host(&clipboard.replace(r#""count":2"#, r#""count":65"#)));
    assert!(host(&clipboard.replace(r#""count":2"#, r#""count":1"#)));
    assert!(!host(&clipboard.replace(r#""count":2"#, r#""count":0"#)));
    assert!(!host(
        &clipboard.replace(r#""protocol":1"#, r#""protocol":2"#)
    ));
    assert!(!host(
        &clipboard.replace(r#""text":"abc""#, r#""text":null"#)
    ));
    let image = r#"{"type":"clipboard-image","protocol":1,"id":1,"index":0,"count":342,"mimeType":"image/png","text":"AQID"}"#;
    assert_eq!(kind_of(Side::Host, image), Some(SignalKind::ClipboardImage));
    assert!(!host(&image.replace(r#""count":342"#, r#""count":343"#)));
    assert!(!host(&image.replace(
        r#""mimeType":"image/png""#,
        r#""mimeType":"image/jpeg""#
    )));
    assert!(!host(
        &image.replace(r#""text":"AQID""#, r#""text":"AQ-ID""#)
    ));
    assert!(!host(&image.replace(r#""mimeType":"image/png","#, "")));
}

#[test]
fn input_status_fields() {
    let base = r#"{"type":"input-status","protocol":1,"generation":4,"receivedPackets":12,"acceptedPackets":11,"failedPackets":1,"droppedPackets":1,"releasedInputs":2,"requestedEvents":14,"appliedEvents":13}"#;
    assert_eq!(kind_of(Side::Host, base), Some(SignalKind::InputStatus));
    assert!(!viewer(base));
    assert!(!host(&base.replace(r#""protocol":1"#, r#""protocol":2"#)));
    assert!(!host(
        &base.replace(r#""generation":4"#, r#""generation":4294967296"#)
    ));
    assert!(!host(
        &base.replace(r#""acceptedPackets":11"#, r#""acceptedPackets":13"#)
    ));
    assert!(!host(
        &base.replace(r#""appliedEvents":13"#, r#""appliedEvents":15"#)
    ));
    assert!(!host(&base.replace(r#""protocol":1"#, r#""protocol":1.5"#)));
    assert!(host(
        base.replace('}', r#","lastErrorCode":-2147483648,"lastSequence":65535}"#)
            .as_str()
    ));
    assert!(!host(
        &base.replace('}', r#","lastErrorCode":-2147483649}"#)
    ));
    assert!(!host(&base.replace('}', r#","lastSequence":65536}"#)));
    // input-status has no exact key set; unknown fields pass through.
    assert!(host(&base.replace('}', r#","unknown":1}"#)));
}

#[test]
fn desktop_state_fields() {
    let base = r#"{"type":"desktop-state","state":"active","generation":1,"reason":""}"#;
    assert!(!host(
        &base.replace(r#""state":"active""#, r#""state":"locked""#)
    ));
    assert!(!host(
        &base.replace(r#""generation":1"#, r#""generation":1.5"#)
    ));
    assert!(!host(
        &base.replace(r#""generation":1"#, r#""generation":-1"#)
    ));
    assert!(host(
        &base.replace(r#""state":"active""#, r#""state":"unavailable""#)
    ));
    assert!(!host(&base.replace(r#"}"#, r#","desktop":"alternate"}"#)));
    assert!(!host(&base.replace(r#"}"#, r#","input":"full"}"#)));
    // secureKind needs a secure desktop.
    assert!(!host(&base.replace(r#"}"#, r#","secureKind":"uac"}"#)));
    assert!(host(
        &base.replace(r#"}"#, r#","desktop":"secure","secureKind":"uac"}"#)
    ));
    assert!(!host(&base.replace(
        r#"}"#,
        r#","desktop":"secure","secureKind":"password"}"#
    )));
    assert!(!host(&base.replace(
        r#"}"#,
        r#","desktop":"secure","secureCapabilities":["view","view"]}"#
    )));
    assert!(!host(&base.replace(
        r#"}"#,
        r#","desktop":"secure","secureCapabilities":["view","deny","input","view"]}"#
    )));
    assert!(!host(&base.replace(
        r#"}"#,
        r#","desktop":"secure","secureCapabilities":["approve"]}"#
    )));
    assert!(!host(&base.replace(
        r#"}"#,
        r#","desktop":"secure","secureCapabilities":null}"#
    )));
    assert!(host(&base.replace(
        r#"}"#,
        r#","desktop":"secure","secureCapabilities":[]}"#
    )));
    assert!(host(&base.replace(r#"}"#, r#","desktop":"none"}"#)));
}

#[test]
fn wrong_json_types_are_rejected() {
    assert!(!host(
        r#"{"type":"desktop-state","state":"active","generation":"1","reason":""}"#
    ));
    assert!(!host(
        r#"{"type":"desktop-state","state":"active","generation":1,"reason":1}"#
    ));
    assert!(!host(
        r#"{"type":"desktop-state","state":"active","generation":1,"reason":null}"#
    ));
    assert!(!viewer(r#"{"type":"offer","sdp":1}"#));
    assert!(!viewer(r#"{"type":"offer","sdp":""}"#));
    assert!(!viewer(r#"{"type":"ice"}"#));
    assert!(!!viewer(r#"{"type":"ice","candidate":null}"#));
    assert!(!viewer(
        r#"{"type":"client-log","seq":1,"atMs":1,"level":"info","event":"e","detail":[]}"#
    ));
    assert!(!viewer(
        r#"{"type":"client-log","seq":1,"atMs":1,"level":"","event":"e","detail":{}}"#
    ));
}

#[test]
fn bufferable_messages() {
    let cases = [
        (Side::Viewer, r#"{"type":"offer","sdp":"x"}"#, true),
        (Side::Viewer, r#"{"type":"ice","candidate":{}}"#, true),
        (Side::Host, r#"{"type":"answer","sdp":"x"}"#, false),
        (
            Side::Viewer,
            r#"{"type":"first-frame","protocol":1,"phase":"request","requestId":"abcdefgh","transport":"wss"}"#,
            true,
        ),
        (
            Side::Viewer,
            r#"{"type":"first-frame","protocol":1,"phase":"presented","requestId":"abcdefgh","generation":1}"#,
            false,
        ),
        (
            Side::Viewer,
            r#"{"type":"media-control","action":"start"}"#,
            true,
        ),
        (
            Side::Viewer,
            r#"{"type":"media-control","action":"stop"}"#,
            false,
        ),
        (
            Side::Viewer,
            r#"{"type":"media-control","action":"follow","protocol":4,"epoch":"0123456789abcdef0123456789abcdef","follow":true}"#,
            true,
        ),
        (Side::Host, r#"{"type":"bye"}"#, false),
    ];
    for (side, text, expect) in cases {
        let message = validate_signal_text(side, text, EPOCH).unwrap_or_else(|_| panic!("{text}"));
        assert_eq!(message.bufferable(), expect, "{text}");
    }
}

#[test]
fn consumed_and_cached_types() {
    let cached = r#"{"type":"host-capabilities","serviceMode":false,"coldBoot":false,"secureDesktop":false,"secureText":false}"#;
    let message = validate_signal_text(Side::Host, cached, EPOCH).expect("caps");
    assert!(message.cached_for_viewers() && message.consumed_by_relay());
    assert_eq!(message.raw(), cached);
    let offer =
        validate_signal_text(Side::Viewer, r#"{"type":"offer","sdp":"x"}"#, EPOCH).expect("offer");
    assert!(!offer.cached_for_viewers() && !offer.consumed_by_relay());
    assert_eq!(offer.field("sdp"), "x");
    assert_eq!(offer.field("missing"), "");
    let bye = validate_signal_text(Side::Host, r#"{"type":"bye"}"#, EPOCH).expect("bye");
    assert!(bye.consumed_by_relay() && !bye.cached_for_viewers());
}

#[test]
fn invalid_says_whether_go_closes_the_socket_or_drops_the_message() {
    // Not a JSON object: Go's decode fails and the socket is closed.
    assert_eq!(
        validate_signal_text(Side::Host, "[1]", EPOCH).unwrap_err(),
        Invalid::Close
    );
    assert_eq!(
        validate_signal_text(Side::Host, "not json", EPOCH).unwrap_err(),
        Invalid::Close
    );
    assert_eq!(
        validate_signal_text(Side::Host, "\"bye\"", EPOCH).unwrap_err(),
        Invalid::Close
    );
    // null decodes to a nil map, which no validator accepts: dropped.
    assert_eq!(
        validate_signal_text(Side::Host, "null", EPOCH).unwrap_err(),
        Invalid::Drop
    );
    // Valid object the validator refuses: dropped.
    assert_eq!(
        validate_signal_text(Side::Viewer, r#"{"type":"bye","x":1}"#, EPOCH).unwrap_err(),
        Invalid::Drop
    );
    assert_eq!(
        validate_signal_text(Side::Host, r#"{"type":"approval-decision"}"#, EPOCH).unwrap_err(),
        Invalid::Drop
    );
    // Over the socket read limit: closed.
    let huge = padded(r#"{"type":"bye"}"#, SIGNAL_LIMIT + 1);
    assert_eq!(
        validate_signal_text(Side::Host, &huge, EPOCH).unwrap_err(),
        Invalid::Close
    );
    // Ping is answered on any signaling side without further checks.
    assert!(validate_signal_text(Side::Viewer, r#"{"type":"ping"}"#, EPOCH).is_ok());
    // Media text: a host's text is dropped; a non-object closes.
    let ack = r#"{"type":"ack","generation":1,"seq":2,"recvTsMs":1}"#;
    assert_eq!(
        validate_media_text(Side::Host, ack, EPOCH).unwrap_err(),
        Invalid::Drop
    );
    assert_eq!(
        validate_media_text(Side::Viewer, "[]", EPOCH).unwrap_err(),
        Invalid::Close
    );
    assert_eq!(
        validate_media_text(Side::Viewer, "null", EPOCH).unwrap_err(),
        Invalid::Drop
    );
    // File text: only ping survives; everything else closes the socket.
    assert_eq!(validate_file_text(r#"{"type":"ping"}"#), Ok(FileText::Ping));
    assert_eq!(
        validate_file_text(r#"{"type":"offer"}"#).unwrap_err(),
        Invalid::Close
    );
    assert_eq!(validate_file_text("null").unwrap_err(), Invalid::Close);
    assert_eq!(validate_file_text("nope").unwrap_err(), Invalid::Close);
}

#[test]
fn binary_frames_close_past_the_read_limit_and_on_file_breaks() {
    assert_eq!(
        validate_signal_binary(&zeros(SIGNAL_LIMIT + 1)).unwrap_err(),
        Invalid::Close
    );
    assert_eq!(
        validate_signal_binary(&zeros(SIGNAL_BINARY_MAX + 1)).unwrap_err(),
        Invalid::Drop
    );
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

#[test]
fn drag_preview_types_accept_null_elements_like_go() {
    let base = format!(
        r#"{{"type":"file-drag-preview","protocol":1,"epoch":"{EPOCH}","dragId":1,"action":"update","drop":{{"x":1,"y":2,"generation":1,"viewportRevision":0}},"types":["text/plain",null]}}"#
    );
    assert!(viewer(&base));
    assert!(!viewer(
        &base.replace(r#"["text/plain",null]"#, r#"[null,1]"#)
    ));
    assert!(!viewer(&base.replace(r#"["text/plain",null]"#, "null")));
    assert!(!viewer(&base.replace(r#"["text/plain",null]"#, "[]")));
}

#[test]
fn secure_capabilities_need_a_secure_desktop() {
    let base = r#"{"type":"desktop-state","state":"active","generation":1,"reason":""}"#;
    assert!(host(&base.replace(
        '}',
        r#","desktop":"secure","secureCapabilities":["view"]}"#
    )));
    assert!(!host(&base.replace(
        '}',
        r#","desktop":"default","secureCapabilities":["view"]}"#
    )));
    assert!(!host(
        &base.replace('}', r#","secureCapabilities":["view"]}"#)
    ));
}

#[test]
fn approval_only_host_capabilities_and_approval_decision_are_rejected() {
    assert!(!host(
        r#"{"type":"host-capabilities","browserApproval":true}"#
    ));
    assert!(!host(
        r#"{"type":"host-capabilities","browserApproval":false}"#
    ));
    assert!(!host(
        r#"{"type":"approval-decision","id":"abc","decision":"allow"}"#
    ));
    assert!(host(
        r#"{"type":"host-capabilities","serviceMode":false,"coldBoot":false,"secureDesktop":false,"secureText":false,"browserApproval":true}"#
    ));
}

/// `len` zero bytes.
fn zeros(len: usize) -> Vec<u8> {
    vec![0; len]
}
