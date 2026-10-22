//! Signalling tickets and the data sockets they admit: bearer and ticket
//! admission, PC and epoch binding, the 12 h expiry, and ending with the
//! issuing socket. Ports TestRoutingTicketsAdmitDataHostsByBearerOrTicket,
//! TestRoutingSignalingTicketRejectsWrongAndForeignTickets,
//! TestRoutingSignalingTicketExpiresAtTwelveHours,
//! TestRoutingSignalingTicketEndsWithItsHostSocket and
//! TestRoutingViewerTicketBoundToEpochAndPCAndCookie.

mod relay_harness;

use bp_relay_core::Route;
use relay_harness::*;
use serde_json::json;

const TWELVE_HOURS_MS: u64 = 12 * 60 * 60 * 1000;

fn admitted(actions: &[bp_relay_core::Action], id: bp_relay_core::ConnId) -> bool {
    sent_types(actions, id).contains(&"hello-ok".to_owned()) && closed(actions, id).is_none()
}

fn refused_data() -> Option<(u16, String)> {
    Some((1008, String::new()))
}

#[test]
fn host_ticket_admits_media_host_only_with_its_text() {
    let mut h = Harness::new();
    let a = identity(1);
    let (host, _) = h.register_host(&a, Some(TOKEN));
    let viewer = h.open_viewer(&host.pc);

    let (id, actions) = h.data_hello(
        Route::HostMediaWs,
        &host.pc,
        &viewer.epoch,
        "media-host",
        &b64(&[0u8; 32]),
    );
    assert_eq!(closed(&actions, id), refused_data());

    let (id, actions) = h.data_hello(
        Route::HostMediaWs,
        &host.pc,
        &viewer.epoch,
        "media-host",
        &host.ticket,
    );
    assert!(admitted(&actions, id));
}

#[test]
fn ticket_text_must_be_thirty_two_bytes() {
    let mut h = Harness::new();
    let a = identity(1);
    let (host, _) = h.register_host(&a, Some(TOKEN));
    let viewer = h.open_viewer(&host.pc);
    let (id, actions) = h.data_hello(
        Route::HostMediaWs,
        &host.pc,
        &viewer.epoch,
        "media-host",
        "abc",
    );
    assert_eq!(closed(&actions, id), refused_data());
}

#[test]
fn viewer_ticket_admits_media_client_and_host_ticket_does_not() {
    let mut h = Harness::new();
    let a = identity(1);
    let (host, _) = h.register_host(&a, Some(TOKEN));
    let viewer = h.open_viewer(&host.pc);

    let (id, actions) = h.data_hello(
        Route::MediaWs,
        &host.pc,
        &viewer.epoch,
        "media-client",
        &host.ticket,
    );
    assert_eq!(
        closed(&actions, id),
        refused_data(),
        "a host ticket is not a viewer ticket"
    );

    let (id, actions) = h.data_hello(
        Route::MediaWs,
        &host.pc,
        &viewer.epoch,
        "media-client",
        &viewer.ticket,
    );
    assert!(admitted(&actions, id));
}

#[test]
fn ticket_is_bound_to_its_own_pc() {
    let mut h = Harness::new();
    let a = identity(1);
    let b = identity(2);
    let (host_a, _) = h.register_host(&a, Some(TOKEN));
    let (host_b, _) = h.register_host(&b, Some(TOKEN));
    let viewer_b = h.open_viewer(&host_b.pc);

    // A's host ticket presented on B's room.
    let (id, actions) = h.data_hello(
        Route::HostMediaWs,
        &host_b.pc,
        &viewer_b.epoch,
        "media-host",
        &host_a.ticket,
    );
    assert_eq!(closed(&actions, id), refused_data());

    let (id, actions) = h.data_hello(
        Route::HostMediaWs,
        &host_b.pc,
        &viewer_b.epoch,
        "media-host",
        &host_b.ticket,
    );
    assert!(admitted(&actions, id));
}

#[test]
fn ticket_stops_working_when_its_host_socket_ends() {
    let mut h = Harness::new();
    let a = identity(1);
    let (host, _) = h.register_host(&a, Some(TOKEN));
    let viewer = h.open_viewer(&host.pc);
    h.close(host.id);

    let (id, actions) = h.data_hello(
        Route::HostMediaWs,
        &host.pc,
        &viewer.epoch,
        "media-host",
        &host.ticket,
    );
    assert_eq!(closed(&actions, id), refused_data());
}

#[test]
fn signalling_ticket_expires_at_twelve_hours() {
    let mut h = Harness::new();
    let a = identity(1);
    let (host, _) = h.register_host(&a, Some(TOKEN));
    let viewer = h.open_viewer(&host.pc);
    let ping = json!({"type": "ping"});

    // Keep both signalling sockets alive past their 90 s idle timeout.
    for _ in 0..719 {
        h.advance(60_000);
        h.text(host.id, &ping);
        h.text(viewer.id, &ping);
    }
    // 43,140,000 ms have passed. Move to one millisecond before expiry.
    h.advance(TWELVE_HOURS_MS - 1 - 719 * 60_000);
    h.text(host.id, &ping);
    h.text(viewer.id, &ping);
    let (id, actions) = h.data_hello(
        Route::HostMediaWs,
        &host.pc,
        &viewer.epoch,
        "media-host",
        &host.ticket,
    );
    assert!(admitted(&actions, id), "valid just before 12 h");

    h.advance(1);
    h.text(host.id, &ping);
    h.text(viewer.id, &ping);
    let (id, actions) = h.data_hello(
        Route::HostMediaWs,
        &host.pc,
        &viewer.epoch,
        "media-host",
        &host.ticket,
    );
    assert_eq!(closed(&actions, id), refused_data(), "refused at 12 h");
}

#[test]
fn viewer_ticket_is_bound_to_the_current_epoch() {
    let mut h = Harness::new();
    let a = identity(1);
    let (host, _) = h.register_host(&a, Some(TOKEN));
    let (v1, _) = h.viewer_hello(&host.pc, false);
    let (v2, _) = h.viewer_hello(&host.pc, false);
    assert_ne!(v1.epoch, v2.epoch);

    // The old epoch no longer opens a media socket at all.
    assert!(
        h.try_upgrade(req(Route::MediaWs, Some(&host.pc), Some(&v1.epoch), true))
            .is_err()
    );
    // The old viewer's ticket is refused on the current epoch.
    let (id, actions) = h.data_hello(
        Route::MediaWs,
        &host.pc,
        &v2.epoch,
        "media-client",
        &v1.ticket,
    );
    assert_eq!(closed(&actions, id), refused_data());
    // The current viewer's ticket is admitted.
    let (id, actions) = h.data_hello(
        Route::MediaWs,
        &host.pc,
        &v2.epoch,
        "media-client",
        &v2.ticket,
    );
    assert!(admitted(&actions, id));
}
