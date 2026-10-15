use super::stats::*;
use std::time::{Duration, Instant};
#[test]
fn all_fallback_reasons_and_offer_elapsed_are_machine_readable() {
    let origin = Instant::now();
    let offer = origin + Duration::from_millis(45);
    for reason in [
        "ice-checking-timeout",
        "no-rtp",
        "rtc-failed",
        "mid-session-drop",
    ] {
        let mut stats = WebRtcStats::default();
        stats.fallback(reason, offer + Duration::from_secs(10), origin, offer);
        let json = serde_json::to_value(stats).unwrap();
        assert_eq!(json["fallbacks"][0]["reason"], reason);
        assert_eq!(json["fallbacks"][0]["offer_to_fallback_ms"], 10000.0);
        assert_eq!(json["fallbacks"][0]["at_ms"], 10045.0);
        assert_eq!(json["selected_transport"], "wss");
    }
}
#[test]
fn timeline_and_fallback_retention_is_bounded() {
    let t = Instant::now();
    let mut stats = WebRtcStats::default();
    for i in 0..1000 {
        stats.state(i, "checking", t, t);
        stats.fallback("ice-checking-timeout", t, t, t);
    }
    assert_eq!(stats.ice_timeline.len(), 128);
    assert_eq!(stats.fallbacks.len(), 64);
    assert_eq!(stats.ice_timeline.front().unwrap().local_epoch, 872);
}
#[test]
fn loss_accounts_for_reordered_recovery_and_duplicates() {
    let t = Instant::now();
    let mut stats = RtpStats::default();
    let mut track = RtpTracker::new(t);
    track.packet(65535, 0, 90000.0, t, &mut stats);
    track.packet(
        65537,
        3000,
        90000.0,
        t + Duration::from_millis(34),
        &mut stats,
    );
    assert_eq!(stats.packets_lost, 1);
    track.packet(
        65536,
        1500,
        90000.0,
        t + Duration::from_millis(35),
        &mut stats,
    );
    assert_eq!(stats.packets_lost, 0);
    track.packet(
        65536,
        1500,
        90000.0,
        t + Duration::from_millis(36),
        &mut stats,
    );
    assert_eq!(stats.packets_lost, 0);
}
#[test]
fn jitter_handles_rtp_timestamp_wrap() {
    let t = Instant::now();
    let mut stats = RtpStats::default();
    let mut track = RtpTracker::new(t);
    track.packet(1, u32::MAX - 899, 90000.0, t, &mut stats);
    track.packet(2, 0, 90000.0, t + Duration::from_millis(10), &mut stats);
    assert!(stats.jitter_ms.unwrap() < 0.001);
}
