//! The audio lane over real sockets: an audio-only run with no recordings,
//! scripted loss and reorder, stop and reattach, and runs that do not opt in.
//! Audio is BPA1 binary on the media socket.
//!
//! Reads are counted, not timed: a test asks for N packets and waits up to
//! `WAIT` for them, so a loaded machine only makes a test slower. Each run's
//! `exit_after` is long enough that the reads finish before the server ends
//! the run. Timed windows are used only for "nothing arrives" checks, where a
//! slow machine can only make the check weaker, never fail it.

mod common;

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use common::*;
use fake_wss_host::audio::{decode_packet, AudioCounters, Packet, Script};
use fake_wss_host::model::Injections;
use fake_wss_host::report::{checks, RunMeta};
use serde_json::json;

/// The longest any one read may take before the test fails.
const WAIT: Duration = Duration::from_secs(5);
/// Replies that come right after a hello or a start.
const REPLY_WAIT: Duration = Duration::from_secs(2);

fn meta(script: Script) -> RunMeta {
    RunMeta {
        constraint: None,
        streams: Vec::new(),
        fps: 60,
        bitrate_bps: 20_000_000,
        exit_after_s: 0.0,
        drain_ms: 300,
        bind: String::new(),
        injections: Injections::default(),
        audio: Some(script),
        rtc: None,
    }
}

/// The media socket for `epoch`, with the hello answered and the peer seen.
async fn attach(addr: SocketAddr, epoch: &str, ticket: &str) -> Ws {
    let mut media = media_open(addr, epoch, ticket).await.expect("media upgrade");
    let reply = next_json(&mut media, REPLY_WAIT)
        .await
        .expect("media hello-ok");
    assert_eq!(reply["type"], "hello-ok", "{reply}");
    let peer = next_json(&mut media, REPLY_WAIT)
        .await
        .expect("media-socket peer");
    assert_eq!(peer["type"], "media-peer", "{peer}");
    media
}

/// The next `count` BPA1 packets on the media socket.
async fn packets(ws: &mut Ws, count: usize) -> Vec<Packet> {
    let deadline = Instant::now() + WAIT;
    let mut out = Vec::with_capacity(count);
    while out.len() < count {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let bytes = next_binary(ws, remaining).await.unwrap_or_else(|| {
            panic!(
                "only {} of {count} packets before the wait ended",
                out.len()
            )
        });
        out.push(decode_packet(&bytes).expect("a valid BPA1 packet"));
    }
    out
}

/// The next packet flagged as a discontinuity. Packets still in flight from
/// before a stop are read past, since they carry no flag.
async fn next_discontinuity(ws: &mut Ws) -> Packet {
    for _ in 0..500 {
        let packet = packets(ws, 1).await.swap_remove(0);
        if packet.discontinuity {
            return packet;
        }
    }
    panic!("no discontinuity within 500 packets");
}

/// Packets that arrive within `window`. Used only for "nothing arrives".
async fn packets_for(ws: &mut Ws, window: Duration) -> Vec<Packet> {
    let deadline = Instant::now() + window;
    let mut out = Vec::new();
    while let Some(bytes) =
        next_binary(ws, deadline.saturating_duration_since(Instant::now())).await
    {
        out.push(decode_packet(&bytes).expect("a valid BPA1 packet"));
    }
    out
}

/// Packets until the socket has been quiet for `quiet`. Bounded, so a lane
/// that keeps sending fails the test instead of hanging it.
async fn packets_until_quiet(ws: &mut Ws, quiet: Duration) {
    for _ in 0..500 {
        if next_binary(ws, quiet).await.is_none() {
            return;
        }
    }
    panic!("audio kept flowing after it should have stopped");
}

/// Whether `seq` is the sequence number of a packet the script drops. Packet
/// index N (1-based) carries sequence N - 1, so a loss every `every` packets
/// takes sequence numbers `every - 1`, `2 * every - 1`, and so on.
fn scripted_lost(seq: u32, every: u32) -> bool {
    (seq + 1).is_multiple_of(every)
}

/// True when the host had no side event that flags a packet, so the only
/// flagged packet in the run is the start. A held reorder packet discarded at
/// the end of the run does not count: it flags nothing.
fn host_clean(audio: &AudioCounters) -> bool {
    audio.resyncs == 0
        && audio.queue_dropped == 0
        && audio.send_failed == 0
        && audio.encode_errors == 0
}

#[tokio::test]
async fn an_audio_only_run_serves_bpa1_with_no_recordings() {
    let harness = spawn_audio(
        Vec::new(),
        Script::default(),
        Duration::from_secs(5),
        Duration::from_millis(300),
    )
    .await;
    let addr = harness.addr;
    let (mut signal, epoch, ticket) = signal_hello_ticket(addr).await;
    let caps = next_json(&mut signal, REPLY_WAIT)
        .await
        .expect("host-capabilities after hello-ok");
    assert_eq!(caps["type"], "host-capabilities", "{caps}");
    assert_eq!(caps["audio"], 1, "{caps}");

    let mut media = attach(addr, &epoch, &ticket).await;
    send_json(&mut signal, start_with_audio(&epoch, "h264")).await;
    // No recording is offered, so the video start is refused; audio still flows.
    let read = packets(&mut media, 60).await;
    drop(signal);
    drop(media);
    let snap = harness.finish().await;

    assert!(
        read[0].discontinuity,
        "the first packet starts a discontinuity"
    );
    assert!(
        read.windows(2)
            .all(|w| w[1].seq == w[0].seq.wrapping_add(1)),
        "no sequence gaps without a script"
    );
    let audio = snap.audio.as_ref().expect("audio counters");
    assert_eq!(audio.opt_in_starts, 1);
    assert!(audio.packets_sent >= read.len() as u64);
    let list = checks(&meta(Script::default()), &snap);
    assert!(list.iter().all(|c| c.pass), "{list:#?}");
}

#[tokio::test]
async fn scripted_loss_and_reorder_show_on_the_wire_and_in_the_counts() {
    let script = Script::new(Some(37), Some(23)).expect("script");
    let harness = spawn_audio(
        Vec::new(),
        script,
        Duration::from_secs(5),
        Duration::from_millis(300),
    )
    .await;
    let addr = harness.addr;
    let (mut signal, epoch, ticket) = signal_hello_ticket(addr).await;
    let mut media = attach(addr, &epoch, &ticket).await;
    send_json(&mut signal, start_with_audio(&epoch, "h264")).await;
    // 80 packets on the wire is more than 37 encoded, so the loss at 37 and the
    // reorder at 23 have both happened by the time these are read.
    let read = packets(&mut media, 80).await;
    drop(signal);
    drop(media);
    let snap = harness.finish().await;

    let audio = snap.audio.as_ref().expect("audio counters");
    assert!(audio.lost_by_script >= 1, "{audio:?}");
    assert_eq!(audio.lost_by_script, audio.packets_encoded / 37);
    assert!(audio.reordered >= 1, "{audio:?}");

    // Scripted loss never reaches the wire, so its sequence number is missing.
    let seqs: Vec<u32> = read.iter().map(|p| p.seq).collect();
    assert!(
        seqs.iter().all(|&s| !scripted_lost(s, 37)),
        "a scripted loss was sent: {seqs:?}"
    );
    assert!(
        seqs.iter().any(|&s| s > 37),
        "the read reaches past the first scripted loss: {seqs:?}"
    );
    // The packet held for reorder at 23 arrives after the one that followed it.
    assert!(
        seqs.windows(2).any(|w| w[1] < w[0]),
        "a reordered packet arrives late: {seqs:?}"
    );

    // Flags depend on host-side events only. The start is the one flagged packet
    // in a clean run: neither the scripted loss nor the reorder flags a packet.
    if host_clean(audio) {
        assert_eq!(read[0].seq, 0, "the first packet is the start");
        assert_eq!(audio.discontinuities_sent, 1, "{audio:?}");
        for p in &read {
            assert_eq!(p.discontinuity, p.seq == 0, "seq {} flag", p.seq);
        }
    } else {
        eprintln!("SKIPPED flag checks: host-side events in this run: {audio:?}");
    }
    let list = checks(&meta(script), &snap);
    assert!(list.iter().all(|c| c.pass), "{list:#?}");
}

#[tokio::test]
async fn a_scripted_loss_is_an_unflagged_sequence_gap_on_the_wire() {
    let script = Script::new(Some(5), None).expect("script");
    let harness = spawn_audio(
        Vec::new(),
        script,
        Duration::from_secs(5),
        Duration::from_millis(300),
    )
    .await;
    let addr = harness.addr;
    let (mut signal, epoch, ticket) = signal_hello_ticket(addr).await;
    let mut media = attach(addr, &epoch, &ticket).await;
    send_json(&mut signal, start_with_audio(&epoch, "h264")).await;
    // 40 packets on the wire is more than 8 losses' worth, so the first loss at
    // index 5 (sequence 4) is well inside the read.
    let read = packets(&mut media, 40).await;
    drop(signal);
    drop(media);
    let snap = harness.finish().await;

    let audio = snap.audio.as_ref().expect("audio counters");
    assert!(audio.lost_by_script >= 1, "{audio:?}");
    let seqs: Vec<u32> = read.iter().map(|p| p.seq).collect();
    assert!(
        seqs.iter().all(|&s| !scripted_lost(s, 5)),
        "a scripted loss was sent: {seqs:?}"
    );
    assert!(
        seqs.iter().any(|&s| s > 4),
        "the read reaches past the first scripted loss: {seqs:?}"
    );

    if host_clean(audio) {
        // The wire has exactly the gaps the script made, and nothing else.
        assert_eq!(read[0].seq, 0, "the first packet is the start");
        let last = *seqs.last().expect("packets were read");
        let expected: Vec<u32> = (0..=last).filter(|&s| !scripted_lost(s, 5)).collect();
        assert_eq!(seqs, expected, "the only gaps are the scripted losses");
        // A loss does not flag the packet after it (sequence 5 follows the loss at
        // sequence 4); only the start is flagged.
        assert_eq!(audio.discontinuities_sent, 1, "{audio:?}");
        for p in &read {
            assert_eq!(p.discontinuity, p.seq == 0, "seq {} flag", p.seq);
        }
    } else {
        eprintln!("SKIPPED gap and flag checks: host-side events in this run: {audio:?}");
    }
    let list = checks(&meta(script), &snap);
    assert!(list.iter().all(|c| c.pass), "{list:#?}");
}

#[tokio::test]
async fn stop_and_media_reattach_each_resume_with_a_discontinuity() {
    let harness = spawn_audio(
        Vec::new(),
        Script::default(),
        Duration::from_secs(8),
        Duration::from_millis(300),
    )
    .await;
    let addr = harness.addr;
    let (mut signal, epoch, ticket) = signal_hello_ticket(addr).await;
    let mut media = attach(addr, &epoch, &ticket).await;

    send_json(&mut signal, start_with_audio(&epoch, "h264")).await;
    let first = packets(&mut media, 3).await;
    assert!(first[0].discontinuity, "the start");

    // A stop on the signalling socket ends audio for the epoch. Packets already
    // on the wire are read out first.
    send_json(
        &mut signal,
        json!({"type": "media-control", "action": "stop", "protocol": 4, "epoch": epoch}),
    )
    .await;
    packets_until_quiet(&mut media, Duration::from_millis(150)).await;

    // A new protocol-4 start with audio:1 resumes it.
    send_json(&mut signal, start_with_audio(&epoch, "h264")).await;
    let restarted = next_discontinuity(&mut media).await;
    assert!(restarted.discontinuity, "a restart after a stop");

    // Dropping the media socket stops audio. The epoch keeps its opt-in, so a
    // reattach resumes with a discontinuity and no new start.
    drop(media);
    let mut media = attach(addr, &epoch, &ticket).await;
    let reattached = next_discontinuity(&mut media).await;
    assert!(reattached.discontinuity, "a reattach after a media drop");
    drop(signal);
    drop(media);
    let snap = harness.finish().await;

    let audio = snap.audio.as_ref().expect("audio counters");
    assert_eq!(audio.opt_in_starts, 2);
    assert!(audio.starts >= 3, "{audio:?}");
    assert!(audio.stops >= 2, "{audio:?}");
    assert!(audio.discontinuities_sent >= 3, "{audio:?}");
    let list = checks(&meta(Script::default()), &snap);
    assert!(list.iter().all(|c| c.pass), "{list:#?}");
}

#[tokio::test]
async fn starts_without_audio_one_get_no_audio_but_the_host_still_advertises_it() {
    let harness = spawn_audio(
        Vec::new(),
        Script::default(),
        Duration::from_secs(2),
        Duration::from_millis(300),
    )
    .await;
    let addr = harness.addr;
    let (mut signal, epoch, ticket) = signal_hello_ticket(addr).await;
    let caps = next_json(&mut signal, REPLY_WAIT)
        .await
        .expect("host-capabilities");
    assert_eq!(caps["audio"], 1, "{caps}");
    let mut media = attach(addr, &epoch, &ticket).await;

    // A plain start, an integer that is not 1, and a string "1": none opts in.
    send_json(&mut signal, start_message(&epoch, "h264")).await;
    let mut wrong_value = start_message(&epoch, "h264");
    wrong_value["audio"] = json!(2);
    send_json(&mut signal, wrong_value).await;
    let mut as_text = start_message(&epoch, "h264");
    as_text["audio"] = json!("1");
    send_json(&mut signal, as_text).await;

    // A "nothing arrives" window: a timed check that can only pass less often
    // on a slow machine if the lane were wrongly on.
    let packets = packets_for(&mut media, Duration::from_millis(500)).await;
    drop(signal);
    drop(media);
    let snap = harness.finish().await;

    assert!(
        packets.is_empty(),
        "{} packets without opt-in",
        packets.len()
    );
    let audio = snap.audio.as_ref().expect("audio counters");
    assert_eq!(audio.opt_in_starts, 0);
    assert_eq!(audio.starts, 0);
    assert_eq!(audio.packets_sent, 0);
    let list = checks(&meta(Script::default()), &snap);
    assert!(
        list.iter().any(|c| c.name == "audio-opted-in" && !c.pass),
        "the opt-in check fails when nothing opted in: {list:#?}"
    );
}

#[tokio::test]
async fn a_new_epoch_does_not_inherit_the_opt_in() {
    let harness = spawn_audio(
        Vec::new(),
        Script::default(),
        Duration::from_secs(5),
        Duration::from_millis(300),
    )
    .await;
    let addr = harness.addr;

    let (mut signal, epoch, ticket) = signal_hello_ticket(addr).await;
    let mut media = attach(addr, &epoch, &ticket).await;
    send_json(&mut signal, start_with_audio(&epoch, "h264")).await;
    assert!(!packets(&mut media, 3).await.is_empty());
    drop(signal);
    drop(media);

    // A fresh session attaches its media socket without a start: nothing flows.
    let (signal, epoch2, ticket2) = signal_hello_ticket(addr).await;
    assert_ne!(epoch, epoch2, "a new hello issues a new epoch");
    let mut media = attach(addr, &epoch2, &ticket2).await;
    let packets = packets_for(&mut media, Duration::from_millis(400)).await;
    drop(signal);
    drop(media);
    let snap = harness.finish().await;

    assert!(
        packets.is_empty(),
        "{} packets in a new epoch",
        packets.len()
    );
    let audio = snap.audio.as_ref().expect("audio counters");
    assert_eq!(audio.opt_in_starts, 1);
}
