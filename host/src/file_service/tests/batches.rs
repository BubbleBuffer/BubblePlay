//! Tests of the file service (see `file_service.rs`).

use super::*;

#[test]
fn uploaded_clipboard_write_does_not_hold_the_input_owners_mutex() {
    let (mut session, _server, _client, shared, dir) = native_test_session();
    session.config.clipboard = true;
    session.batch.saved = vec![dir.join("synthetic.txt")];
    session.batch.purpose = Some(Purpose::Paste);
    session.finish_batch_with(&shared, |_| {
        // The native call can block arbitrarily. Foreground/window work
        // must not prevent the input owner from reading or changing state.
        let mut state = shared
            .try_lock()
            .expect("clipboard write held session mutex");
        rebind(&mut state, Some("replacement-viewer"));
        Ok(42)
    });
    let state = lock(&shared);
    assert_eq!(state.own_clipboard_seq, Some(42));
    assert_eq!(
        state.outbox.pastes, 0,
        "old viewer must not paste into a new session"
    );
    drop(state);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn live_paste_batch_queues_one_paste_for_the_bound_viewer() {
    let (mut session, _server, _client, shared, dir) = native_test_session();
    session.config.clipboard = true;
    session.batch.saved = vec![dir.join("synthetic.txt")];
    session.batch.purpose = Some(Purpose::Paste);
    session.finish_batch_with(&shared, |_| Ok(9));
    let state = lock(&shared);
    assert_eq!(state.own_clipboard_seq, Some(9));
    assert_eq!(state.outbox.pastes, 1);
    drop(state);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn stale_batch_never_reaches_the_clipboard_or_queues_a_paste() {
    let (mut session, _server, _client, shared, dir) = native_test_session();
    session.config.clipboard = true;
    session.batch.saved = vec![dir.join("synthetic.txt")];
    session.batch.purpose = Some(Purpose::Paste);
    rebind(&mut lock(&shared), Some("replacement-viewer"));
    let mut called = false;
    session.finish_batch_with(&shared, |_| {
        called = true;
        Ok(7)
    });
    assert!(!called, "a stale batch must not call the clipboard");
    let state = lock(&shared);
    assert_eq!(state.own_clipboard_seq, None);
    assert_eq!(state.outbox.pastes, 0);
    drop(state);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn clearing_the_epoch_discards_the_old_outbox_and_pending_native_drops() {
    let (_session, _server, _client, shared, dir) = native_test_session();
    let batch = DropBatch::new(vec![drop_meta(0, 1)]).unwrap();
    let point = ft::DropPoint {
        x: 12,
        y: 34,
        generation: 1,
        viewport_revision: 2,
    };
    {
        let mut state = lock(&shared);
        state.outbox.signals.push("old-offer".into());
        state.outbox.pastes = 2;
        state
            .outbox
            .drops
            .push(NativeDropRequest::new(batch, point, "test".into()));
    }
    let stale = rebind(&mut lock(&shared), None);
    assert_eq!(stale.len(), 1, "the pending drop is handed back to reject");
    for request in stale {
        request.reject("viewer-changed");
    }
    let state = lock(&shared);
    assert_eq!(state.epoch, None);
    assert!(state.outbox.signals.is_empty());
    assert_eq!(state.outbox.pastes, 0);
    assert!(state.outbox.drops.is_empty());
    drop(state);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn stale_socket_session_stops_before_reading_and_removes_its_partial_upload() {
    let (mut session, mut ws, _client, shared, dir) = native_test_session();
    session.receiving = Some(receiving(&dir, "pending.bin", 3));
    let partial = session.receiving.as_ref().unwrap().partial.clone();
    rebind(&mut lock(&shared), Some("replacement-viewer"));
    let stop = AtomicBool::new(false);
    session.serve(&mut ws, &shared, &stop).unwrap();
    session.abort_all("socket-closed");
    assert!(!partial.exists());
    assert!(session.receiving.is_none());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn stale_viewer_packets_are_not_processed_while_the_bound_viewer_gets_them() {
    let (mut session, mut ws, _client, shared, dir) = native_test_session();
    announce(&shared, &[meta("a.bin", 3)], digest(b"abc"));
    let offer = ft::encode(&Packet {
        transfer_id: 41,
        seq: 0,
        body: Body::Offer(meta("a.bin", 3)),
    });
    session.on_packet(&mut ws, &offer, &shared).unwrap();
    assert!(
        session.receiving.is_some(),
        "control: the bound viewer's offer is accepted"
    );
    session.abort_all("socket-closed");
    rebind(&mut lock(&shared), Some("replacement-viewer"));
    session.on_packet(&mut ws, &offer, &shared).unwrap();
    assert!(session.receiving.is_none());
    assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
    fs::remove_dir_all(dir).unwrap();
}
