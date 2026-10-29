//! The scripted cursor sequence (state 128, kind 129) on `/ws`: sent after the
//! browser's accepted `start`, in order, with the exact wire bytes that
//! `host/src/input.rs` and the worker decoders expect.

mod common;

use std::time::Duration;

use common::*;

const WAIT: Duration = Duration::from_secs(2);

/// Each packet's bytes, written out by hand from the wire layouts.
fn expected() -> Vec<Vec<u8>> {
    vec![
        // state: no ack, x 0x8000, y 0x8000, in bounds + visible
        vec![128, 0, 0, 0x00, 0x80, 0x00, 0x80, 6],
        // kind: Text
        vec![129, 1],
        // state: ack 7 valid, x 0, y 0, in bounds + visible + ack valid
        vec![128, 7, 0, 0, 0, 0, 0, 7],
        // kind: Pointer
        vec![129, 2],
        // state: ack 0xFFFF valid, x and y 0xFFFF, visible + ack valid, out of bounds
        vec![128, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 5],
        // kind: Hidden
        vec![129, 14],
        // state: no ack, x 1, y 2, neither in bounds nor visible
        vec![128, 0, 0, 1, 0, 2, 0, 0],
        // kind: Arrow
        vec![129, 0],
    ]
}

#[tokio::test]
async fn cursor_script_is_sent_on_ws_after_start_in_order_with_exact_bytes() {
    let harness = spawn(
        synthetic(60, 30),
        no_injections(),
        Duration::from_millis(3000),
        Duration::from_millis(100),
    )
    .await;
    let (mut signal, _media, _epoch) = streaming_session(harness.addr, "h264").await;

    for (i, want) in expected().iter().enumerate() {
        let got = next_binary(&mut signal, WAIT)
            .await
            .unwrap_or_else(|| panic!("cursor packet {i} never arrived"));
        assert_eq!(&got, want, "cursor packet {i}");
    }
    // The script is finite: nothing more follows it.
    assert!(
        next_binary(&mut signal, Duration::from_millis(300))
            .await
            .is_none(),
        "no cursor packet after the script"
    );
    harness.finish().await;
}

#[tokio::test]
async fn cursor_script_waits_for_the_browser_start() {
    let harness = spawn(
        synthetic(60, 30),
        no_injections(),
        Duration::from_millis(1000),
        Duration::from_millis(100),
    )
    .await;
    let (mut signal, _epoch) = signal_hello(harness.addr).await;
    assert!(
        next_binary(&mut signal, Duration::from_millis(400))
            .await
            .is_none(),
        "hello alone sends no cursor packets"
    );
    harness.finish().await;
}
