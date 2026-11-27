//! Tests of the file service (see `file_service.rs`).

use super::*;

#[test]
fn prepared_large_upload_rejects_second_batch_and_bad_hash_without_delivery() {
    let (mut session, mut ws, mut client, shared, dir) = native_test_session();
    let mut large = drop_meta(0, 1);
    large.size = crate::native_file_drop::PREPARE_NATIVE_DROP_BYTES;
    announce(&shared, std::slice::from_ref(&large), [7; 32]);
    session
        .on_offer(&mut ws, 41, large.clone(), &shared)
        .unwrap();
    read_packet(&mut client);
    let path = session
        .drop_upload
        .as_ref()
        .unwrap()
        .batch
        .as_ref()
        .unwrap()
        .path(0);
    let request = lock(&shared).outbox.drops.pop().unwrap();
    assert!(request.preparation_required());
    assert_eq!(request.preparation_status(), Ok(false));
    let mut next = drop_meta(0, 1);
    next.batch_id = format!("{:032x}", 2);
    session.on_offer(&mut ws, 42, next, &shared).unwrap();
    match ft::decode(&read_packet(&mut client)).unwrap().body {
        Body::Cancel(payload) => assert_eq!(payload.reason, "busy"),
        other => panic!("unexpected {other:?}"),
    }
    let chunk = vec![0; ft::MAX_DATA_BYTES];
    for seq in 0..(large.size / chunk.len() as u64) {
        session
            .on_data(&mut ws, 41, seq as u32, &chunk, &shared)
            .unwrap();
    }
    assert_eq!(
        request.preparation_status(),
        Ok(false),
        "bytes alone never make the batch deliverable"
    );
    session.on_done(&mut ws, 41, &[0; 32], &shared).unwrap();
    assert_eq!(request.preparation_status(), Err("hash-mismatch".into()));
    match ft::decode(&read_non_ack(&mut client)).unwrap().body {
        Body::Cancel(payload) => assert_eq!(payload.reason, "hash-mismatch"),
        other => panic!("unexpected {other:?}"),
    }
    request.reject("test-cleanup");
    assert!(session.drop_upload.is_none());
    assert!(!path.exists());
    assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
    fs::remove_dir(&dir).unwrap();
}
#[test]
fn native_batch_starts_at_metadata_and_results_wait_for_every_hash() {
    let (mut session, mut ws, mut client, shared, dir) = native_test_session();
    announce(&shared, &[drop_meta(0, 2), drop_meta(1, 2)], digest(b"abc"));
    session
        .on_offer(&mut ws, 1, drop_meta(0, 2), &shared)
        .unwrap();
    assert!(matches!(
        ft::decode(&read_packet(&mut client)).unwrap().body,
        Body::Accept
    ));
    assert!(lock(&shared).outbox.drops.is_empty());
    session
        .on_offer(&mut ws, 2, drop_meta(1, 2), &shared)
        .unwrap();
    read_packet(&mut client);
    assert_eq!(lock(&shared).outbox.drops.len(), 1);
    // A fake target accepts; hashes still gate RESULT for the entire batch.
    session
        .drop_upload
        .as_ref()
        .unwrap()
        .batch
        .as_ref()
        .unwrap()
        .accepted_for_test();
    let mut hash = Sha256::new().unwrap();
    hash.update(b"abc");
    let digest = hash.finish();
    session.on_data(&mut ws, 1, 0, b"abc", &shared).unwrap();
    session.on_done(&mut ws, 1, &digest, &shared).unwrap();
    assert!(matches!(
        ft::decode(&read_packet(&mut client)).unwrap().body,
        Body::Ack(3)
    ));
    assert_eq!(lock(&shared).stats.uploads_ok, 0);
    assert_eq!(
        fs::read_dir(&dir).unwrap().count(),
        0,
        "drop files never reach Downloads"
    );
    session.on_data(&mut ws, 2, 0, b"abc", &shared).unwrap();
    session.on_done(&mut ws, 2, &digest, &shared).unwrap();
    for _ in 0..2 {
        assert!(matches!(
            ft::decode(&read_non_ack(&mut client)).unwrap().body,
            Body::Result(ResultPayload { ok: true, .. })
        ));
    }
    assert_eq!(lock(&shared).stats.uploads_ok, 2);
    assert_eq!(lock(&shared).outbox.pastes, 0);
    lock(&shared).outbox.drops.clear();
    fs::remove_dir(&dir).unwrap();
}
#[test]
fn native_target_reject_and_hash_failure_cancel_and_remove_private_cache() {
    for reject_target in [true, false] {
        let (mut session, mut ws, mut client, shared, dir) = native_test_session();
        announce(&shared, &[drop_meta(0, 1)], digest(b"abc"));
        session
            .on_offer(&mut ws, 7, drop_meta(0, 1), &shared)
            .unwrap();
        read_packet(&mut client);
        let path = session
            .drop_upload
            .as_ref()
            .unwrap()
            .batch
            .as_ref()
            .unwrap()
            .path(0);
        let request = lock(&shared).outbox.drops.pop().unwrap();
        if reject_target {
            request.reject("drop-unavailable");
            session.poll_drop(&mut ws, &shared).unwrap();
        } else {
            session.on_data(&mut ws, 7, 0, b"abc", &shared).unwrap();
            session.on_done(&mut ws, 7, &[0; 32], &shared).unwrap();
            request.reject("test-cleanup");
        }
        let bytes = read_non_ack(&mut client);
        match ft::decode(&bytes).unwrap().body {
            Body::Cancel(payload) => assert_eq!(
                payload.reason,
                if reject_target {
                    "drop-unavailable"
                } else {
                    "hash-mismatch"
                }
            ),
            other => panic!("unexpected {other:?}"),
        }
        assert!(session.drop_upload.is_none());
        assert!(!path.exists());
        assert_eq!(lock(&shared).stats.uploads_failed, 1);
        fs::remove_dir(&dir).unwrap();
    }
}
