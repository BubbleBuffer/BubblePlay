//! Browser startup rules, with one explicitly authorized native deviation.
//! `media.js:767-865` requires connected ICE and DTLS, usable stats, no video
//! RTP, and five seconds since BOTH the request and the latest progress.
//! `receiver.js:332-353` retries the first-frame watchdog once after five seconds.
use std::time::{Duration, Instant};

use crate::net::TransportChoice;

pub const FIRST_FRAME_INTERVAL: Duration = Duration::from_secs(5);
pub const NO_RTP_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    RetryFirstFrame,
    FirstFrameFailed,
    Fallback(&'static str),
}

pub struct Startup {
    choice: TransportChoice,
    checking_timeout: Duration,
    offer_at: Option<Instant>,
    request_at: Option<Instant>,
    progress_at: Instant,
    first_deadline: Option<Instant>,
    retried: bool,
    failed: bool,
    pub ice_connected: bool,
    pub peer_connected: bool,
    pub stats_available: bool,
    pub video_packets: u64,
    pub video_bytes: u64,
    pub frames: u64,
    pub ready: bool,
    pub using_wss: bool,
}

impl Startup {
    pub fn new(choice: TransportChoice, checking_timeout: Duration, now: Instant) -> Self {
        Self {
            choice,
            checking_timeout,
            offer_at: None,
            request_at: None,
            progress_at: now,
            first_deadline: None,
            retried: false,
            failed: false,
            ice_connected: false,
            peer_connected: false,
            stats_available: false,
            video_packets: 0,
            video_bytes: 0,
            frames: 0,
            ready: false,
            using_wss: false,
        }
    }
    pub fn offer_sent(&mut self, at: Instant) {
        self.offer_at = Some(at);
        self.request_sent(at);
    }
    pub fn request_sent(&mut self, at: Instant) {
        self.request_at = Some(at);
        self.first_deadline = Some(at + FIRST_FRAME_INTERVAL);
    }
    pub fn progress(&mut self, at: Instant) {
        self.progress_at = at;
    }
    pub fn decoded(&mut self) {
        self.frames += 1;
    }
    pub fn presented(&mut self) {
        self.ready = true;
        self.first_deadline = None;
    }
    /// A failed offer-time handshake is renewed when slow ICE finally connects,
    /// matching receiver.js:354-360 and :2015. An in-flight request is retained.
    pub fn connected(&mut self, at: Instant) -> bool {
        self.progress(at);
        if self.failed {
            self.failed = false;
            self.retried = false;
            self.request_sent(at);
            return true;
        }
        false
    }
    pub fn terminal(&mut self) -> Option<Action> {
        (self.choice == TransportChoice::Auto && !self.using_wss).then_some(Action::Fallback(
            if self.frames > 0 || self.ready {
                "mid-session-drop"
            } else {
                "rtc-failed"
            },
        ))
    }
    pub fn select_wss(&mut self) {
        self.using_wss = true;
        self.first_deadline = None;
    }
    pub fn tick(&mut self, now: Instant) -> Vec<Action> {
        if self.using_wss {
            return Vec::new();
        }
        let mut actions = Vec::new();
        // Unlike the browser (media.js:824), the native client does not wait
        // indefinitely in checking. This deadline is measured from successful
        // offer transmission, never from answer receipt or ICE progress.
        if self.choice == TransportChoice::Auto
            && !self.ice_connected
            && self
                .offer_at
                .is_some_and(|at| now.saturating_duration_since(at) >= self.checking_timeout)
        {
            return vec![Action::Fallback("ice-checking-timeout")];
        }
        if self.first_deadline.is_some_and(|at| now >= at) && !self.ready {
            if !self.retried {
                self.retried = true;
                self.request_sent(now);
                actions.push(Action::RetryFirstFrame);
            } else {
                self.failed = true;
                self.first_deadline = None;
                actions.push(Action::FirstFrameFailed);
            }
        }
        if self.choice == TransportChoice::Auto
            && self.ice_connected
            && self.peer_connected
            && self.stats_available
            && self.frames == 0
            && self.video_packets == 0
            && self.video_bytes == 0
            && self
                .request_at
                .is_some_and(|at| now.saturating_duration_since(at) >= NO_RTP_INTERVAL)
            && now.saturating_duration_since(self.progress_at) >= NO_RTP_INTERVAL
        {
            actions.push(Action::Fallback("no-rtp"));
        }
        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::AUTO_ICE_CHECKING_TIMEOUT;
    fn setup(choice: TransportChoice) -> (Startup, Instant) {
        let t = Instant::now();
        let mut s = Startup::new(choice, AUTO_ICE_CHECKING_TIMEOUT, t);
        s.offer_sent(t);
        (s, t)
    }
    #[test]
    fn checking_deadline_is_from_offer_and_progress_does_not_extend_it() {
        let (mut s, t) = setup(TransportChoice::Auto);
        s.progress(t + AUTO_ICE_CHECKING_TIMEOUT - Duration::from_millis(1000));
        assert!(!s
            .tick(t + AUTO_ICE_CHECKING_TIMEOUT - Duration::from_millis(1))
            .iter()
            .any(|a| matches!(a, Action::Fallback(_))));
        assert_eq!(
            s.tick(t + AUTO_ICE_CHECKING_TIMEOUT),
            vec![Action::Fallback("ice-checking-timeout")]
        );
        s.select_wss();
        assert!(s.tick(t + Duration::from_secs(100)).is_empty());
    }
    #[test]
    fn healthy_rtc_stays_selected_after_startup_deadlines() {
        let (mut s, t) = setup(TransportChoice::Auto);
        s.ice_connected = true;
        s.peer_connected = true;
        s.stats_available = true;
        s.connected(t + Duration::from_millis(200));
        s.decoded();
        s.presented();
        assert!(s.tick(t + Duration::from_secs(10)).is_empty());
        assert!(s.tick(t + Duration::from_secs(60)).is_empty());
        assert!(!s.using_wss);
    }
    #[test]
    fn forced_rtc_does_not_fall_back_on_checking_or_terminal_failure() {
        let (mut s, t) = setup(TransportChoice::WebRtc);
        assert_eq!(
            s.tick(t + Duration::from_secs(10)),
            vec![Action::RetryFirstFrame]
        );
        assert_eq!(s.terminal(), None);
    }
    #[test]
    fn first_frame_watchdog_retries_once_then_fails_without_early_wss() {
        let (mut s, t) = setup(TransportChoice::Auto);
        s.ice_connected = true;
        s.peer_connected = true;
        assert!(s.tick(t + Duration::from_millis(4999)).is_empty());
        assert_eq!(
            s.tick(t + Duration::from_secs(5)),
            vec![Action::RetryFirstFrame]
        );
        assert_eq!(
            s.tick(t + Duration::from_secs(10)),
            vec![Action::FirstFrameFailed]
        );
        assert!(s.tick(t + Duration::from_secs(15)).is_empty());
    }
    #[test]
    fn no_rtp_requires_established_peer_known_stats_and_full_progress_budget() {
        let (mut s, t) = setup(TransportChoice::Auto);
        s.ice_connected = true;
        s.peer_connected = true;
        s.stats_available = true;
        s.progress(t + Duration::from_secs(6));
        assert_eq!(
            s.tick(t + Duration::from_secs(5)),
            vec![Action::RetryFirstFrame]
        );
        assert_eq!(
            s.tick(t + Duration::from_secs(10)),
            vec![Action::FirstFrameFailed]
        );
        assert_eq!(
            s.tick(t + Duration::from_secs(11)),
            vec![Action::Fallback("no-rtp")]
        );
    }
    #[test]
    fn packets_bytes_or_frames_each_suppress_no_rtp_fallback() {
        for which in 0..3 {
            let (mut s, t) = setup(TransportChoice::Auto);
            s.ice_connected = true;
            s.peer_connected = true;
            s.stats_available = true;
            match which {
                0 => s.video_packets = 1,
                1 => s.video_bytes = 1,
                _ => s.decoded(),
            }
            s.tick(t + Duration::from_secs(5));
            assert!(!s
                .tick(t + Duration::from_secs(10))
                .iter()
                .any(|a| matches!(a, Action::Fallback(_))));
        }
    }
    #[test]
    fn terminal_failure_rescues_initial_and_mid_session_rtc() {
        for healthy in [false, true] {
            let (mut s, _) = setup(TransportChoice::Auto);
            if healthy {
                s.decoded();
                s.presented();
            }
            assert_eq!(
                s.terminal(),
                Some(Action::Fallback(if healthy {
                    "mid-session-drop"
                } else {
                    "rtc-failed"
                }))
            );
            s.select_wss();
            assert_eq!(s.terminal(), None);
        }
    }
    #[test]
    fn late_ice_renews_failed_handshake_but_retains_inflight_request() {
        let (mut s, t) = setup(TransportChoice::WebRtc);
        assert!(!s.connected(t + Duration::from_secs(1)));
        s.tick(t + Duration::from_secs(5));
        s.tick(t + Duration::from_secs(10));
        assert!(s.connected(t + Duration::from_secs(12)));
        assert!(s.tick(t + Duration::from_secs(16)).is_empty());
        assert_eq!(
            s.tick(t + Duration::from_secs(17)),
            vec![Action::RetryFirstFrame]
        );
    }
}
