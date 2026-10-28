//! Traffic on live sockets: relay-consumed messages, drops versus closes,
//! read limits before auth, file pairs, and host replacement. Ports the
//! forwarding and boundary subtests of routing_boundaries_test.go and
//! server_test.go.

mod relay_harness;

use bp_relay_core::{HELLO_LIMIT, Route, SIGNAL_LIMIT};
use relay_harness::*;
use serde_json::json;

#[test]
fn unknown_signal_message_is_dropped_and_the_socket_kept() {
    let mut h = Harness::new();
    let a = identity(1);
    let (host, _) = h.register_host(&a, Some(TOKEN));
    let out = h.raw(host.id, r#"{"type":"nope"}"#);
    assert!(out.is_empty());
    let out = h.text(host.id, &json!({"type": "ping"}));
    assert_eq!(sent_types(&out, host.id), vec!["pong".to_owned()]);
}

#[test]
fn signal_text_that_is_not_an_object_closes_with_invalid_packet() {
    let mut h = Harness::new();
    let a = identity(1);
    let (host, _) = h.register_host(&a, Some(TOKEN));
    let out = h.raw(host.id, "[1,2]");
    assert_eq!(
        closed(&out, host.id),
        Some((1008, "invalid packet".to_owned()))
    );
}

#[test]
fn oversized_text_on_a_live_socket_closes_with_peer_disconnected() {
    let mut h = Harness::new();
    let a = identity(1);
    let (host, _) = h.register_host(&a, Some(TOKEN));
    let big = format!(
        "{{\"type\":\"ping\",\"pad\":\"{}\"}}",
        "x".repeat(SIGNAL_LIMIT)
    );
    let out = h.raw(host.id, &big);
    assert_eq!(
        closed(&out, host.id),
        Some((1001, "peer disconnected".to_owned()))
    );
}

#[test]
fn pre_auth_text_over_the_hello_limit_closes_invalid_hello() {
    let mut h = Harness::new();
    let a = identity(1);
    let id = h.upgrade(req(Route::HostWs, Some(&a.id().to_string()), None, false));
    let big = "x".repeat(HELLO_LIMIT + 1);
    let out = h.raw(id, &big);
    assert_eq!(closed(&out, id), Some((1008, "invalid hello".to_owned())));
}

#[test]
fn pre_auth_binary_closes_invalid_hello() {
    let mut h = Harness::new();
    let a = identity(1);
    let id = h.upgrade(req(Route::HostWs, Some(&a.id().to_string()), None, false));
    let out = h.binary(id, &[1, 2, 3]);
    assert_eq!(closed(&out, id), Some((1008, "invalid hello".to_owned())));
}

#[test]
fn signal_binary_is_not_forwarded_in_either_direction() {
    let mut h = Harness::new();
    let a = identity(1);
    let (host, _) = h.register_host(&a, Some(TOKEN));
    let viewer = h.open_viewer(&host.pc);
    for frame in [vec![1u8; 1], vec![1u8; 10], vec![1u8; 16384], Vec::new()] {
        let out = h.binary(viewer.id, &frame);
        assert!(
            sent_binary(&out, host.id).is_empty(),
            "viewer frame relayed"
        );
        assert!(
            closes(&out, viewer.id).is_empty(),
            "viewer frame closed the socket"
        );
        let out = h.binary(host.id, &frame);
        assert!(
            sent_binary(&out, viewer.id).is_empty(),
            "host frame relayed"
        );
        assert!(
            closes(&out, host.id).is_empty(),
            "host frame closed the socket"
        );
    }
}

#[test]
fn file_socket_accepts_only_ping_text() {
    let mut h = Harness::new();
    let a = identity(1);
    let (host, _) = h.register_host(&a, Some(TOKEN));
    let viewer = h.open_viewer(&host.pc);
    let (file, actions) = h.data_hello(
        Route::FileWs,
        &host.pc,
        &viewer.epoch,
        "file-viewer",
        &viewer.ticket,
    );
    assert!(sent_types(&actions, file).contains(&"hello-ok".to_owned()));

    let out = h.text(file, &json!({"type": "ping"}));
    assert_eq!(sent_types(&out, file), vec!["pong".to_owned()]);
    let out = h.text(file, &json!({"type": "hello"}));
    assert_eq!(
        closed(&out, file),
        Some((1008, "invalid packet".to_owned()))
    );
}

#[test]
fn invalid_file_frame_closes_the_pair() {
    let mut h = Harness::new();
    let a = identity(1);
    let (host, _) = h.register_host(&a, Some(TOKEN));
    let viewer = h.open_viewer(&host.pc);
    let (file_host, _) = h.data_hello(
        Route::HostFileWs,
        &host.pc,
        &viewer.epoch,
        "file-host",
        &host.ticket,
    );
    let (file_viewer, _) = h.data_hello(
        Route::FileWs,
        &host.pc,
        &viewer.epoch,
        "file-viewer",
        &viewer.ticket,
    );

    // Sixteen zero bytes are not a BPF1 header.
    let out = h.binary(file_viewer, &[0u8; 16]);
    assert_eq!(
        closed(&out, file_host),
        Some((1001, "file-peer-left".to_owned()))
    );
    assert_eq!(
        closed(&out, file_viewer),
        Some((1008, "invalid packet".to_owned()))
    );
}

#[test]
fn host_replacement_closes_the_old_host_and_tells_the_viewer() {
    let mut h = Harness::new();
    let a = identity(1);
    let (old_host, _) = h.register_host(&a, Some(TOKEN));
    let viewer = h.open_viewer(&old_host.pc);

    let (new_host, actions) = h.register_host(&a, None);
    assert_eq!(
        closed(&actions, old_host.id),
        Some((1008, "peer replaced".to_owned()))
    );
    assert_eq!(
        sent_types(&actions, viewer.id),
        vec!["peer-left".to_owned()]
    );

    let ctl = r#"{"type":"ctl","kind":"sdp-offer","seq":0,"data":"AAAA"}"#;
    let out = h.raw(viewer.id, ctl);
    assert_eq!(sent_text(&out, new_host.id), vec![ctl.to_owned()]);
    assert!(sent_text(&out, old_host.id).is_empty());
}

#[test]
fn signalling_ping_gets_pong_before_any_viewer_joins() {
    let mut h = Harness::new();
    let a = identity(1);
    let (host, _) = h.register_host(&a, Some(TOKEN));
    let out = h.text(host.id, &json!({"type": "ping"}));
    assert_eq!(sent_types(&out, host.id), vec!["pong".to_owned()]);
}
