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
    for wrong in [
        meta("a.bin", 4),
        meta("b.bin", 3),
        OfferMeta {
            purpose: Purpose::Paste,
            ..meta("a.bin", 3)
        },
        OfferMeta {
            mime: "text/plain".into(),
            ..meta("a.bin", 3)
        },
        OfferMeta {
            mtime_ms: 9,
            ..meta("a.bin", 3)
        },
    ] {
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
    // Guessing an id is an unknown offer; the right id nobody asked for is unauthorized.
    for (guess, expected) in [
        (format!("{:032x}", 1), "unknown-offer"),
        (offer_id.clone(), "unauthorized"),
    ] {
        session
            .requests
            .push_back((9, request(&guess), Instant::now()));
        session.start_next_request(&mut ws, &shared).unwrap();
        assert_eq!(cancel_reason(&mut client), expected);
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

/// A clipboard writer that counts its calls (one counter per test).
macro_rules! placement_counter {
    () => {
        static PLACED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        fn place(_: &[PathBuf]) -> Result<u32> {
            PLACED.fetch_add(1, Ordering::SeqCst);
            Ok(1)
        }
    };
}

fn batch_meta(index: u32, count: u32) -> OfferMeta {
    OfferMeta {
        name: format!("f{index}.bin"),
        purpose: Purpose::Paste,
        batch_index: index,
        batch_count: count,
        ..meta("x", 3)
    }
}

fn announce_paste_batch(shared: &Mutex<Shared>, count: u32) {
    let metas: Vec<OfferMeta> = (0..count).map(|index| batch_meta(index, count)).collect();
    announce(shared, &metas, digest(b"abc"));
}

#[test]
fn a_batch_missing_one_of_its_files_is_never_placed_or_pasted() {
    placement_counter!();
    let (mut session, mut ws, mut client, shared, dir) = native_test_session();
    session.config.clipboard = true;
    session.set_files = place;
    announce_paste_batch(&shared, 2);
    // The relay dropped file 0 and delivered only the last-index file.
    let last = batch_meta(1, 2);
    let abc = digest(b"abc");
    session
        .on_packet(&mut ws, &offer_packet(41, &last), &shared)
        .unwrap();
    read_packet(&mut client);
    session.on_data(&mut ws, 41, 0, b"abc", &shared).unwrap();
    session.on_done(&mut ws, 41, &abc, &shared).unwrap();
    assert!(matches!(
        ft::decode(&read_non_ack(&mut client)).unwrap().body,
        Body::Result(ResultPayload { ok: true, .. })
    ));
    assert_eq!(PLACED.load(Ordering::SeqCst), 0);
    assert_eq!(lock(&shared).outbox.pastes, 0);
    // Control: the missing file arrives after all, and the batch is placed once.
    session
        .on_packet(&mut ws, &offer_packet(42, &batch_meta(0, 2)), &shared)
        .unwrap();
    read_packet(&mut client);
    session.on_data(&mut ws, 42, 0, b"abc", &shared).unwrap();
    session.on_done(&mut ws, 42, &abc, &shared).unwrap();
    assert_eq!(PLACED.load(Ordering::SeqCst), 1);
    assert_eq!(lock(&shared).outbox.pastes, 1);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_failed_file_abandons_the_whole_batch() {
    placement_counter!();
    let (mut session, mut ws, mut client, shared, dir) = native_test_session();
    session.config.clipboard = true;
    session.set_files = place;
    announce_paste_batch(&shared, 3);
    let abc = digest(b"abc");
    for (id, index) in [(41, 0), (42, 1)] {
        session
            .on_packet(&mut ws, &offer_packet(id, &batch_meta(index, 3)), &shared)
            .unwrap();
        read_packet(&mut client);
        // File 1 carries other bytes than announced.
        let (bytes, hash) = if index == 1 {
            (&b"abd"[..], digest(b"abd"))
        } else {
            (&b"abc"[..], abc)
        };
        session.on_data(&mut ws, id, 0, bytes, &shared).unwrap();
        session.on_done(&mut ws, id, &hash, &shared).unwrap();
        read_non_ack(&mut client);
    }
    session
        .on_packet(&mut ws, &offer_packet(43, &batch_meta(2, 3)), &shared)
        .unwrap();
    assert_eq!(cancel_reason(&mut client), "unannounced");
    assert_eq!(PLACED.load(Ordering::SeqCst), 0);
    assert_eq!(lock(&shared).outbox.pastes, 0);
    assert!(session.batch.saved.is_empty());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_cancelled_or_expired_batch_stops_its_upload_and_removes_the_partial_file() {
    let (mut session, mut ws, mut client, shared, dir) = native_test_session();
    announce_paste_batch(&shared, 1);
    session
        .on_packet(&mut ws, &offer_packet(41, &batch_meta(0, 1)), &shared)
        .unwrap();
    read_packet(&mut client);
    session.on_data(&mut ws, 41, 0, b"a", &shared).unwrap();
    session.sweep_dead_batches(&mut ws, &shared).unwrap();
    assert!(session.receiving.is_some(), "a live batch is left alone");
    // `file-batch-cancel` over the sealed channel forgets the batch.
    lock(&shared).auth.forget_batch(&format!("{:032x}", 1));
    session.sweep_dead_batches(&mut ws, &shared).unwrap();
    assert_eq!(cancel_reason(&mut client), "cancelled");
    assert!(session.receiving.is_none());
    assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_drop_offer_refused_after_its_announcement_frees_the_batch() {
    let (mut session, mut ws, mut client, shared, dir) = native_test_session();
    let metas = [drop_meta(0, 2), drop_meta(1, 2)];
    announce(&shared, &metas, digest(b"abc"));
    session
        .on_packet(&mut ws, &offer_packet(5, &metas[0]), &shared)
        .unwrap();
    read_packet(&mut client);
    // The same transfer id again for the other file of the batch.
    session
        .on_packet(&mut ws, &offer_packet(5, &metas[1]), &shared)
        .unwrap();
    assert_eq!(cancel_reason(&mut client), "invalid-drop");
    assert!(session.drop_upload.is_none());
    assert!(!lock(&shared)
        .auth
        .has_batch(&format!("{:032x}", 1), Instant::now()));
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_batch_announced_before_the_viewer_is_bound_is_not_ready() {
    let (_session, _ws, _client, shared, dir) = native_test_session();
    let batch = || BatchAnnouncement {
        batch_id: format!("{:032x}", 1),
        purpose: Purpose::Save,
        drop: None,
        files: vec![AnnouncedFile {
            name: "a.bin".into(),
            size: 3,
            mime: String::new(),
            mtime_ms: 0,
            sha256: digest(b"abc"),
        }],
    };
    rebind(&mut lock(&shared), None);
    assert_eq!(lock(&shared).announce(batch()), Err("not-ready"));
    rebind(&mut lock(&shared), Some("test"));
    assert_eq!(lock(&shared).announce(batch()), Ok(()));
    fs::remove_dir_all(dir).unwrap();
}
