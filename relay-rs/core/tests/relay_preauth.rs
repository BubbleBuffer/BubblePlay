//! Sockets that have not finished their hello cannot lock a PC out: they are capped on their
//! own, a full relay drops the oldest of them, and the connection cap counts only the sockets
//! that finished their handshake.

mod relay_harness;

use std::net::{IpAddr, Ipv4Addr};

use bp_relay_core::relay::{EVICTED_CLOSE_CODE, PRE_AUTH_MAX_TOTAL};
use bp_relay_core::{Action, Reject, Route};
use relay_harness::*;

const ATTACKER: IpAddr = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 9));
/// More upgrades than any pending cap allows.
const FLOOD: usize = 40;

#[test]
fn pending_flood_does_not_lock_out_the_host() {
    let mut h = Harness::new();
    let a = identity(1);
    let pc = a.id().to_string();
    h.client = ATTACKER;
    let mut dropped = 0;
    for _ in 0..FLOOD {
        let upgraded = h
            .relay
            .upgrade(req(Route::HostWs, Some(&pc), None, false), ATTACKER, h.now)
            .expect("a pending flood is never refused");
        for action in upgraded.evicted {
            if let Action::Close(id, code, _) = action {
                assert_eq!(code, EVICTED_CLOSE_CODE);
                dropped += 1;
                h.close(id);
            }
        }
    }
    assert!(
        dropped >= FLOOD - PRE_AUTH_MAX_TOTAL,
        "the flood drops its own sockets"
    );
    h.client = CLIENT;
    let (host, _) = h.register_host(&a, Some(TOKEN));
    assert!(h.relay.pc_online(&host.pc));
}

#[test]
fn connection_cap_counts_only_finished_handshakes() {
    let mut h = Harness::with(|c| c.max_connections = 2);
    let a = identity(1);
    let pc = a.id().to_string();
    let (_host, _) = h.register_host(&a, Some(TOKEN));
    // One slot is left: a pending socket takes it, a newer pending socket replaces it.
    let first = h.upgrade(req(Route::Ws, Some(&pc), None, true));
    let second = h
        .relay
        .upgrade(req(Route::Ws, Some(&pc), None, true), h.client, h.now)
        .expect("a pending socket replaces a pending one");
    assert_eq!(second.evicted.len(), 1);
    assert!(matches!(&second.evicted[0], Action::Close(id, ..) if *id == first));
    // The host and the admitted viewer fill both slots: nothing is left to drop.
    h.close(first);
    h.open_viewer(&pc);
    let refused = h
        .relay
        .upgrade(req(Route::Ws, Some(&pc), None, true), h.client, h.now);
    assert_eq!(refused, Err(Reject));
}
