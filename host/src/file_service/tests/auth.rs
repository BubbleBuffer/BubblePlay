//! Tests of the file service (see `file_service.rs`).

use super::*;

#[test]
fn an_unannounced_offer_is_refused_and_leaves_nothing_behind() {
    let (mut session, mut ws, mut client, shared, dir) = native_test_session();
    session
        .on_packet(&mut ws, &offer_packet(41, &meta("planted.bin", 3)), &shared)
        .unwrap();
    assert_eq!(cancel_reason(&mut client), "unannounced");
    assert!(session.receiving.is_none());
    let mut drop_offer = drop_meta(0, 1);
    drop_offer.batch_id = format!("{:032x}", 9);
    session
        .on_packet(&mut ws, &offer_packet(42, &drop_offer), &shared)
        .unwrap();
    assert_eq!(cancel_reason(&mut client), "unannounced");
    assert!(session.drop_upload.is_none());
    assert!(lock(&shared).outbox.drops.is_empty());
    assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn an_offer_that_differs_from_its_announcement_is_refused() {
    let (mut session, mut ws, mut client, shared, dir) = native_test_session();
    announce(&shared, &[meta("a.bin", 3)], digest(b"abc"));
    for wrong in [meta("a.bin", 4), meta("b.bin", 3), {
        let mut paste = meta("a.bin", 3);
        paste.purpose = Purpose::Paste;
        paste
    }] {
        session
            .on_packet(&mut ws, &offer_packet(41, &wrong), &shared)
            .unwrap();
        assert_eq!(cancel_reason(&mut client), "unannounced");
        assert!(session.receiving.is_none());
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn bytes_that_do_not_match_the_announced_hash_never_reach_the_clipboard_or_paste() {
    let (mut session, mut ws, mut client, shared, dir) = native_test_session();
    let mut paste = meta("a.bin", 3);
    paste.purpose = Purpose::Paste;
    announce(&shared, std::slice::from_ref(&paste), digest(b"abc"));
    session
        .on_packet(&mut ws, &offer_packet(41, &paste), &shared)
        .unwrap();
    assert!(matches!(
        ft::decode(&read_packet(&mut client)).unwrap().body,
        Body::Accept
    ));
    // Right size, other bytes, and a DONE hash that matches those bytes (not the announcement).
    session.on_data(&mut ws, 41, 0, b"abd", &shared).unwrap();
    session
        .on_done(&mut ws, 41, &digest(b"abd"), &shared)
        .unwrap();
    match ft::decode(&read_non_ack(&mut client)).unwrap().body {
        Body::Result(result) => {
            assert!(!result.ok);
            assert_eq!(result.error.as_deref(), Some("hash-mismatch"));
        }
        other => panic!("unexpected {other:?}"),
    }
    assert!(session.batch.saved.is_empty());
    let state = lock(&shared);
    assert_eq!(state.stats.uploads_ok, 0);
    assert_eq!(state.outbox.pastes, 0);
    assert!(state.own_clipboard_seq.is_none());
    drop(state);
    assert_eq!(
        fs::read_dir(&dir).unwrap().count(),
        0,
        "no partial or final file"
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn an_announced_file_is_saved_once_and_a_replay_of_its_batch_is_refused() {
    let (mut session, mut ws, mut client, shared, dir) = native_test_session();
    let upload = meta("a.bin", 3);
    announce(&shared, std::slice::from_ref(&upload), digest(b"abc"));
    session
        .on_packet(&mut ws, &offer_packet(41, &upload), &shared)
        .unwrap();
    read_packet(&mut client);
    session.on_data(&mut ws, 41, 0, b"abc", &shared).unwrap();
    session
        .on_done(&mut ws, 41, &digest(b"abc"), &shared)
        .unwrap();
    match ft::decode(&read_non_ack(&mut client)).unwrap().body {
        Body::Result(result) => assert!(result.ok),
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(fs::read(dir.join("a.bin")).unwrap(), b"abc");
    session
        .on_packet(&mut ws, &offer_packet(42, &upload), &shared)
        .unwrap();
    assert_eq!(cancel_reason(&mut client), "unannounced");
    assert!(session.receiving.is_none());
    assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_request_is_served_only_for_an_item_the_viewer_asked_for_over_control() {
    let (mut session, mut ws, mut client, shared, dir) = native_test_session();
    session.request_grace = Duration::ZERO;
    let file = dir.join("offered.bin");
    fs::write(&file, b"secret").unwrap();
    publish_offer(
        &shared,
        "clipboard",
        vec![OfferedFile {
            id: 1,
            path: file,
            name: "offered.bin".into(),
            size: 6,
        }],
    );
    let offer_id = lock(&shared).offer.as_ref().unwrap().id.clone();
    let request = |offer_id: &str| RequestPayload {
        offer_id: offer_id.into(),
        id: 1,
    };
    // Guessing an id, and the right id nobody asked for.
    for guess in [format!("{:032x}", 1), offer_id.clone()] {
        session
            .requests
            .push_back((9, request(&guess), Instant::now()));
        session.start_next_request(&mut ws, &shared).unwrap();
        let reason = cancel_reason(&mut client);
        assert!(
            reason == "unknown-offer" || reason == "unauthorized",
            "{reason}"
        );
        assert!(session.sending.is_none());
    }
    // Control: asked for over the sealed channel, served once.
    lock(&shared).auth.grant(&offer_id, &[1], Instant::now());
    session
        .requests
        .push_back((10, request(&offer_id), Instant::now()));
    session.start_next_request(&mut ws, &shared).unwrap();
    assert!(matches!(
        ft::decode(&read_packet(&mut client)).unwrap().body,
        Body::Offer(_)
    ));
    session.sending = None;
    session
        .requests
        .push_back((11, request(&offer_id), Instant::now()));
    session.start_next_request(&mut ws, &shared).unwrap();
    assert_eq!(cancel_reason(&mut client), "unauthorized");
    fs::remove_dir_all(dir).unwrap();
}
