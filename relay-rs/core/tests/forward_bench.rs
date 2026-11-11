//! CPU cost of the core's forward path for one media frame, without any adapter around it.
//! Ignored by default. Run it to compare changes to the hot path:
//!   cargo test --release -p bp-relay-core --test forward_bench -- --ignored --nocapture
//! It prints nanoseconds per frame for a 188-byte host frame forwarded to its viewer (the size of
//! one 10 ms Opus packet with its header). The worker's own end-to-end number is
//! `relay-rs/worker/scripts/bench-forward.mjs`.

mod relay_harness;

use std::time::Instant;

use bp_relay_core::{Action, Route};
use relay_harness::*;

#[test]
#[ignore = "a benchmark: run it by hand with --release"]
fn forward_one_media_frame() {
    let mut h = Harness::new();
    let identity = identity(1);
    let (host, _) = h.register_host(&identity, Some(TOKEN));
    let viewer = h.open_viewer(&host.pc);
    let (media_host, _) = h.data_hello(
        Route::HostMediaWs,
        &host.pc,
        &viewer.epoch,
        "media-host",
        &host.ticket,
    );
    let (media_viewer, _) = h.data_hello(
        Route::MediaWs,
        &host.pc,
        &viewer.epoch,
        "media-viewer",
        &viewer.ticket,
    );
    let frame = vec![7u8; 188];
    let sent = h.binary(media_host, &frame);
    assert!(
        sent.iter()
            .any(|a| matches!(a, Action::SendBinary(id, _) if *id == media_viewer)),
        "the frame reaches the viewer"
    );
    const FRAMES: u32 = 200_000;
    let mut forwarded = 0u32;
    let mut best = u128::MAX;
    for _ in 0..5 {
        let started = Instant::now();
        for _ in 0..FRAMES {
            let now = h.now;
            let actions = h.relay.on_binary(media_host, &frame, now);
            forwarded += actions.len() as u32;
            std::hint::black_box(&actions);
        }
        best = best.min(started.elapsed().as_nanos() / u128::from(FRAMES));
    }
    assert!(forwarded > 0);
    println!("core forward: {best} ns per frame (best of 5 rounds of {FRAMES})");
}
