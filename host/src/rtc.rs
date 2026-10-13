//! str0m WebRTC session wrapper: SDP answer to browser offer, candidate
//! plumbing, media write path, PLI/BWE event extraction.
//!
//! The host answers (browser offers), matching the locked decision. The video
//! m-line's `Mid` and the H.264 payload type come from `Event::MediaAdded`
//! (str0m fires it for browser-offered media too).

use std::time::Instant;

use anyhow::{Context, Result};
use str0m::bwe::BweKind;
use str0m::change::SdpOffer;
use str0m::channel::ChannelId;
use str0m::format::Codec;
use str0m::media::MediaTime;
use str0m::media::{Frequency, KeyframeRequest, MediaKind, Mid, Pt};
use str0m::{Candidate, Event, Rtc};
use tracing::{debug, info, warn};

use crate::audio::AudioFrame;
use crate::encode::VideoCodec;

/// Keep Windows timed channel waits usable for the RTP pacer's deadlines.
/// Acquire only for a negotiated RTC epoch; Drop also covers errors/teardown.
/// RAII `timeBeginPeriod(1)` guard, shared by the RTC and WSS v4 media paths.
pub struct RtcTimerPeriod;

impl RtcTimerPeriod {
    pub fn acquire() -> Option<Self> {
        // No pointer arguments; pair each successful request with timeEndPeriod.
        let status = unsafe { windows::Win32::Media::timeBeginPeriod(1) };
        if status == 0 {
            Some(Self)
        } else {
            warn!(status, "RTC high resolution timer request failed");
            None
        }
    }
}

impl Drop for RtcTimerPeriod {
    fn drop(&mut self) {
        unsafe {
            windows::Win32::Media::timeEndPeriod(1);
        }
    }
}

fn preferred_negotiated_codec(hevc: Option<Pt>, h264: Option<Pt>) -> Option<(VideoCodec, Pt)> {
    hevc.map(|pt| (VideoCodec::Hevc, pt))
        .or_else(|| h264.map(|pt| (VideoCodec::H264, pt)))
}

/// Events the run loop cares about, extracted from str0m's Output::Event.
pub enum RtcAction {
    IceConnected,
    IceDisconnected,
    /// Reliable input lane opened (including an in-session reopen).
    InputControlReady,
    /// Binary data from the input channel.
    InputData(Vec<u8>),
    /// PLI/FIR — force an IDR.
    KeyframeRequested,
    /// Egress bandwidth estimate (bps).
    Bwe(u64),
    /// Codec selected from the browser offer. The producer recreates NVENC
    /// before admitting media for this RTC epoch.
    VideoCodecSelected(VideoCodec),
    /// The browser offer carries a send-direction Opus track for this epoch.
    /// The coordinator may submit PC audio from now on.
    AudioReady,
    Closed,
}

/// Last audio frame written on this epoch's track, for the next RTP timestamp.
#[derive(Debug, Clone, Copy)]
struct AudioTiming {
    seq: u32,
    capture_ts_us: u64,
    rtp_48k: u64,
}

pub struct RtcSession {
    pub rtc: Rtc,
    pub video_mid: Option<Mid>,
    pub video_pt: Option<Pt>,
    pub video_codec: Option<VideoCodec>,
    pub audio_mid: Option<Mid>,
    pub audio_pt: Option<Pt>,
    pub ice_connected: bool,
    /// ICE reported Disconnected; a later Connected is a recovery.
    ice_lost: bool,
    /// Base instant for the 90 kHz RTP clock.
    clock_start: Instant,
    /// Last 90 kHz media time written, kept strictly increasing so every
    /// access unit written before an IDR has an earlier RTP timestamp.
    last_rtp_90k: Option<u64>,
    /// Audio uses its own 48 kHz clock; see `next_audio_rtp_48k`.
    last_audio: Option<AudioTiming>,
    frames_written: u64,
    input_control: Option<ChannelId>,
    input_motion: Option<ChannelId>,
    timer_period: Option<RtcTimerPeriod>,
}

impl RtcSession {
    pub fn new(now: Instant, initial_bitrate_bps: u32) -> Self {
        let rtc = Rtc::builder()
            .enable_h265(true)
            // Initial BWE estimate must match the encoder's starting bitrate
            // (8 Mbps default) so congestion control doesn't fight the encoder
            // out of the gate; TWCC/REMB feedback then adjusts from there.
            .enable_bwe(Some(str0m::bwe::Bitrate::bps(initial_bitrate_bps as u64)))
            .build(now);
        Self {
            rtc,
            video_mid: None,
            video_pt: None,
            video_codec: None,
            audio_mid: None,
            audio_pt: None,
            ice_connected: false,
            ice_lost: false,
            clock_start: now,
            last_rtp_90k: None,
            last_audio: None,
            frames_written: 0,
            input_control: None,
            input_motion: None,
            timer_period: None,
        }
    }

    /// Add our host candidate (must happen before accept_offer so it is baked
    /// into the answer).
    pub fn add_host_candidate(&mut self, addr: std::net::SocketAddr) -> Result<()> {
        let c = Candidate::host(addr, "udp").context("host candidate")?;
        self.rtc.add_local_candidate(c);
        info!(%addr, "added local host candidate");
        Ok(())
    }

    /// Accept the browser's SDP offer; returns the SDP answer string.
    pub fn accept_offer(&mut self, offer_sdp: &str) -> Result<String> {
        let offer = SdpOffer::from_sdp_string(offer_sdp).context("parse offer SDP")?;
        let answer = self
            .rtc
            .sdp_api()
            .accept_offer(offer)
            .context("accept_offer")?;
        self.clock_start = Instant::now();
        self.last_rtp_90k = None;
        if self.timer_period.is_none() {
            self.timer_period = RtcTimerPeriod::acquire();
        }

        Ok(answer.to_sdp_string())
    }

    /// Add a discovered server-reflexive local candidate (post-offer trickle).
    pub fn add_srflx_candidate(
        &mut self,
        mapped: std::net::SocketAddr,
        base: std::net::SocketAddr,
    ) -> Result<Candidate> {
        let c = Candidate::server_reflexive(mapped, base, "udp").context("srflx candidate")?;
        self.rtc.add_local_candidate(c.clone());
        info!(%mapped, %base, "added local srflx candidate");
        Ok(c)
    }

    /// Add a remote (browser) trickled candidate, given as an SDP `a=candidate`
    /// attribute line (with or without the `a=` prefix).
    pub fn add_remote_candidate(&mut self, sdp: &str) {
        match Candidate::from_sdp_string(sdp) {
            Ok(c) => {
                self.rtc.add_remote_candidate(c);
                debug!(sdp, "added remote candidate");
            }
            Err(e) => warn!(sdp, ?e, "ignoring bad remote candidate"),
        }
    }

    /// Handle one str0m event; returns an action for the run loop if relevant.
    pub fn on_event(&mut self, event: Event) -> Option<RtcAction> {
        match event {
            Event::Connected => {
                self.ice_connected = true;
                info!("ICE connected");
                Some(RtcAction::IceConnected)
            }
            Event::IceConnectionStateChange(state) => {
                use str0m::IceConnectionState::*;
                match state {
                    // str0m's ICE state machine has no Failed/Closed variants;
                    // Disconnected is the terminal "lost the peer" signal here.
                    Disconnected => {
                        self.ice_connected = false;
                        self.ice_lost = true;
                        warn!(?state, "ICE disconnected");
                        Some(RtcAction::IceDisconnected)
                    }
                    Connected | Completed if self.ice_lost => {
                        self.ice_lost = false;
                        self.ice_connected = true;
                        info!(?state, "ICE reconnected");
                        Some(RtcAction::IceConnected)
                    }
                    _ => None,
                }
            }
            Event::MediaAdded(added) => {
                if added.kind == MediaKind::Audio && added.direction.is_sending() {
                    return self.select_audio(added.mid);
                }
                if added.kind == MediaKind::Video && added.direction.is_sending() {
                    self.video_mid = Some(added.mid);
                    // HEVC is the policy default. H.264 High remains the
                    // negotiated compatibility fallback for this epoch.
                    let mut hevc: Option<Pt> = None;
                    let mut fallback: Option<Pt> = None;
                    let mut high: Option<Pt> = None;
                    if let Some(writer) = self.rtc.writer(added.mid) {
                        for p in writer.payload_params() {
                            if p.spec().codec == str0m::format::Codec::H265 {
                                hevc.get_or_insert(p.pt());
                                continue;
                            }
                            if p.spec().codec != str0m::format::Codec::H264 {
                                continue;
                            }
                            let plid = p.spec().format.profile_level_id.unwrap_or(0);
                            let profile = plid >> 16;
                            if profile == 0x64 {
                                high.get_or_insert(p.pt());
                            } else if fallback.is_none() {
                                fallback = Some(p.pt());
                            }
                        }
                    }
                    if let Some((codec, pt)) = preferred_negotiated_codec(hevc, high.or(fallback)) {
                        self.video_codec = Some(codec);
                        self.video_pt = Some(pt);
                        info!(mid = %added.mid, ?pt, codec = codec.name(), "video send media added");
                        return Some(RtcAction::VideoCodecSelected(codec));
                    }
                    warn!(mid = %added.mid, "browser offered no supported HEVC/H264 video payload");
                }
                None
            }
            Event::ChannelOpen(id, label) => {
                info!(channel_id = ?id, label = %label, "data channel opened by peer");
                match label.as_str() {
                    "input-control" => {
                        self.input_control = Some(id);
                        return Some(RtcAction::InputControlReady);
                    }
                    "input-motion" => self.input_motion = Some(id),
                    _ => return None,
                }
                None
            }
            Event::ChannelClose(id) => {
                info!(channel_id = ?id, "data channel closed by peer");
                if self.input_control == Some(id) {
                    self.input_control = None;
                }
                if self.input_motion == Some(id) {
                    self.input_motion = None;
                }
                None
            }
            Event::ChannelData(data) => {
                debug!(channel_id = ?data.id, binary = data.binary, len = data.data.len(), "channel data");
                if !data.binary
                    || (Some(data.id) != self.input_control && Some(data.id) != self.input_motion)
                {
                    return None;
                }
                Some(RtcAction::InputData(data.data))
            }
            Event::KeyframeRequest(KeyframeRequest { kind, .. }) => {
                // This is an important recovery signal, not routine packet
                // noise: the browser's bounded-latency decoder explicitly
                // requests an IDR after repeated late frames.
                info!(
                    ?kind,
                    "receiver keyframe request (PLI/FIR); scheduling IDR recovery"
                );
                Some(RtcAction::KeyframeRequested)
            }
            Event::EgressBitrateEstimate(bwe) => {
                let bps = match bwe {
                    BweKind::Twcc(bitrate) => bitrate.as_u64(),
                    BweKind::Remb(_, bitrate) => bitrate.as_u64(),
                    _ => return None, // future BWE kinds: ignore until needed
                };
                Some(RtcAction::Bwe(bps))
            }
            Event::PeerStats(stats) => {
                debug!(?stats, "peer stats");
                None
            }
            Event::Closed => Some(RtcAction::Closed),
            _ => None,
        }
    }

    /// Write one Annex B access unit as an RTP media packet.
    ///
    /// Returns the 32-bit RTP timestamp only when the access unit was actually
    /// admitted to the negotiated video writer. This keeps transport telemetry
    /// honest during connection setup and teardown. str0m copies the 90 kHz
    /// media time verbatim into the RTP header (truncated to 32 bits), so the
    /// returned value is the timestamp the browser sees for this frame.
    pub fn write_video(&mut self, access_unit: Vec<u8>) -> Result<Option<u32>> {
        let (Some(mid), Some(pt)) = (self.video_mid, self.video_pt) else {
            return Ok(None); // media not negotiated yet
        };
        let Some(writer) = self.rtc.writer(mid) else {
            return Ok(None);
        };
        let elapsed = self.clock_start.elapsed();
        let rtp_90k = next_rtp_90k(self.last_rtp_90k, elapsed);
        writer.write(
            pt,
            Instant::now(),
            MediaTime::from_90khz(rtp_90k),
            access_unit,
        )?;
        self.last_rtp_90k = Some(rtp_90k);
        self.frames_written += 1;
        Ok(Some(rtp_90k as u32))
    }

    /// Select the Opus payload type of a send-direction audio track.
    fn select_audio(&mut self, mid: Mid) -> Option<RtcAction> {
        let writer = self.rtc.writer(mid)?;
        let pt = writer
            .payload_params()
            .find(|p| p.spec().codec == Codec::Opus)
            .map(|p| p.pt());
        let Some(pt) = pt else {
            warn!(%mid, "browser offered no Opus audio payload");
            return None;
        };
        self.audio_mid = Some(mid);
        self.audio_pt = Some(pt);
        self.last_audio = None;
        info!(%mid, ?pt, "audio send media added");
        Some(RtcAction::AudioReady)
    }

    /// Write one 10 ms Opus frame on the audio track. Returns `Ok(false)` when
    /// the frame is not admitted: no negotiated Opus track yet, or ICE not
    /// connected (str0m drops media written before then). Nothing is recorded
    /// for a frame that was not admitted, so the next one reads as a gap.
    pub fn write_audio(&mut self, frame: AudioFrame) -> Result<bool> {
        let (Some(mid), Some(pt)) = (self.audio_mid, self.audio_pt) else {
            return Ok(false);
        };
        if !self.ice_connected {
            return Ok(false);
        }
        let Some(writer) = self.rtc.writer(mid) else {
            return Ok(false);
        };
        let (rtp_48k, talkspurt) = next_audio_rtp_48k(
            self.last_audio,
            frame.seq,
            frame.capture_ts_us,
            frame.discontinuity,
        );
        writer.start_of_talkspurt(talkspurt).write(
            pt,
            Instant::now(),
            MediaTime::new(rtp_48k, Frequency::FORTY_EIGHT_KHZ),
            frame.payload,
        )?;
        self.last_audio = Some(AudioTiming {
            seq: frame.seq,
            capture_ts_us: frame.capture_ts_us,
            rtp_48k,
        });
        Ok(true)
    }

    /// Write authoritative cursor feedback on the browser-created input lane.
    pub fn write_cursor_state(&mut self, reliable: bool, data: &[u8]) -> Result<bool> {
        let id = if reliable {
            self.input_control
        } else {
            self.input_motion
        };
        let Some(id) = id else {
            return Ok(false);
        };
        let Some(mut channel) = self.rtc.channel(id) else {
            return Ok(false);
        };
        channel
            .write(true, data)
            .context("write cursor state data channel")
    }

    pub fn frames_written(&self) -> u64 {
        self.frames_written
    }

    pub fn alive(&self) -> bool {
        self.rtc.is_alive()
    }
}

/// 90 kHz media time for the next video access unit: the session clock, but
/// always after the previous write. Two writes inside one 11 us tick would
/// otherwise share an RTP timestamp, and the browser's first-frame gate relies
/// on every frame before an admitted IDR carrying an earlier timestamp.
fn next_rtp_90k(last: Option<u64>, elapsed: std::time::Duration) -> u64 {
    let clock = (elapsed.as_secs_f64() * 90_000.0) as u64;
    match last {
        Some(last) if clock <= last => last + 1,
        _ => clock,
    }
}

/// PC audio is 10 ms Opus frames (BPA1 contract), so one sequence step is 480
/// samples on the 48 kHz RTP clock.
const AUDIO_CLOCK_HZ: u64 = 48_000;
const AUDIO_FRAME_SAMPLES: u64 = 480;
/// Forward sequence steps are read in the first half of the u32 space; anything
/// beyond that is a backwards or duplicate sequence (RFC 1982 style), not a gap.
const AUDIO_MAX_FORWARD_STEPS: u32 = i32::MAX as u32;

/// 48 kHz RTP media time and talkspurt marker for the next audio frame. The
/// timeline advances by the silence a gap represents, so the browser's playout
/// keeps the sample intervals. A forward sequence gap advances by its frames.
/// A discontinuity, restart or duplicate advances by the full capture-clock
/// elapsed samples, with no upper limit. The marker starts a talkspurt after
/// any gap or discontinuity.
fn next_audio_rtp_48k(
    last: Option<AudioTiming>,
    seq: u32,
    capture_ts_us: u64,
    discontinuity: bool,
) -> (u64, bool) {
    let Some(last) = last else {
        return (0, true);
    };
    let steps = seq.wrapping_sub(last.seq);
    let contiguous = steps == 1 && !discontinuity;
    let samples = if !discontinuity && (1..=AUDIO_MAX_FORWARD_STEPS).contains(&steps) {
        steps as u64 * AUDIO_FRAME_SAMPLES
    } else {
        // Capture-clock elapsed samples (floor), never less than one frame.
        let elapsed_us = capture_ts_us.saturating_sub(last.capture_ts_us);
        let elapsed_samples =
            (u128::from(elapsed_us) * u128::from(AUDIO_CLOCK_HZ) / 1_000_000) as u64;
        elapsed_samples.max(AUDIO_FRAME_SAMPLES)
    };
    (last.rtp_48k + samples, !contiguous)
}

#[cfg(test)]
mod tests {
    use super::{next_rtp_90k, preferred_negotiated_codec};
    use std::time::Duration;

    #[test]
    fn rtp_media_time_is_strictly_increasing() {
        assert_eq!(next_rtp_90k(None, Duration::from_secs(1)), 90_000);
        // Same 11 us tick, or a clock that did not advance, still moves forward.
        assert_eq!(next_rtp_90k(Some(90_000), Duration::from_secs(1)), 90_001);
        assert_eq!(next_rtp_90k(Some(95_000), Duration::from_secs(1)), 95_001);
        assert_eq!(next_rtp_90k(Some(90_000), Duration::from_secs(2)), 180_000);
    }
    use crate::encode::VideoCodec;
    use str0m::media::Pt;

    #[test]
    fn negotiated_codec_prefers_hevc_and_falls_back_to_h264() {
        let hevc = Pt::new_with_value(102);
        let h264 = Pt::new_with_value(96);
        assert_eq!(
            preferred_negotiated_codec(Some(hevc), Some(h264)),
            Some((VideoCodec::Hevc, hevc))
        );
        assert_eq!(
            preferred_negotiated_codec(None, Some(h264)),
            Some((VideoCodec::H264, h264))
        );
        assert_eq!(preferred_negotiated_codec(None, None), None);
    }

    fn timing(seq: u32, capture_ts_us: u64, rtp_48k: u64) -> super::AudioTiming {
        super::AudioTiming {
            seq,
            capture_ts_us,
            rtp_48k,
        }
    }

    #[test]
    fn audio_clock_advances_by_sample_intervals() {
        use super::next_audio_rtp_48k;
        let last = timing(10, 0, 96_000);
        // Contiguous 10 ms frames: 480 samples, no marker.
        assert_eq!(
            next_audio_rtp_48k(Some(last), 11, 10_000, false),
            (96_480, false)
        );
        // Sequence wrap is contiguous too.
        let wrap = timing(u32::MAX, 0, 96_000);
        assert_eq!(
            next_audio_rtp_48k(Some(wrap), 0, 10_000, false),
            (96_480, false)
        );
        // Three lost frames (a silence gap) advance by four frames and mark a talkspurt.
        assert_eq!(
            next_audio_rtp_48k(Some(last), 14, 40_000, false),
            (96_000 + 4 * 480, true)
        );
        // A discontinuity follows the capture clock: 1 s of capture time is 48000 samples.
        assert_eq!(
            next_audio_rtp_48k(Some(last), 11, 1_000_000, true),
            (144_000, true)
        );
        // Discontinuity without elapsed capture time still moves one frame.
        assert_eq!(next_audio_rtp_48k(Some(last), 11, 0, true), (96_480, true));
        // Restart with a new sequence base: capture clock, not the wrapped sequence step.
        assert_eq!(
            next_audio_rtp_48k(Some(last), 3_000_000_000, 10_000, false),
            (96_480, true)
        );
        // A long silent gap advances by its full capture-clock samples, uncapped:
        // 60 s is 2_880_000 samples, 10 min is 28_800_000.
        assert_eq!(
            next_audio_rtp_48k(Some(last), 11, 60_000_000, true),
            (96_000 + 2_880_000, true)
        );
        assert_eq!(
            next_audio_rtp_48k(Some(last), 11, 600_000_000, true),
            (96_000 + 28_800_000, true)
        );
        // A forward sequence gap of 1000 frames (10 s) bridges by sequence, uncapped.
        assert_eq!(
            next_audio_rtp_48k(Some(last), 1010, 10_000_000, false),
            (96_000 + 1000 * 480, true)
        );
        // Duplicate sequence still moves forward.
        assert_eq!(next_audio_rtp_48k(Some(last), 10, 0, false), (96_480, true));
        // First frame of an epoch starts the track at zero with a marker.
        assert_eq!(next_audio_rtp_48k(None, 7, 123_456_789, true), (0, true));
    }

    /// The `m=audio` section of an SDP, up to the next media section.
    fn audio_section(sdp: &str) -> &str {
        let start = sdp.find("m=audio").expect("answer has an audio m-line");
        sdp[start..].split("\nm=").next().unwrap_or("")
    }

    /// Payload type of the `opus/48000/2` rtpmap line in an SDP section.
    fn opus_payload_type(section: &str) -> Option<u8> {
        section.lines().find_map(|line| {
            let (pt, codec) = line.strip_prefix("a=rtpmap:")?.split_once(' ')?;
            if codec.eq_ignore_ascii_case("opus/48000/2") {
                pt.parse().ok()
            } else {
                None
            }
        })
    }

    /// Tests SDP negotiation and the host's MediaAdded handling, not a full
    /// ICE/DTLS exchange. str0m's poll_event stays silent until the session is
    /// ready for SRTP, so the MediaAdded event is fed in explicitly, using the
    /// negotiated mid, direction and payload type from the answer.
    #[test]
    fn recvonly_audio_offer_negotiates_opus_send_track() {
        use str0m::media::{Direction, MediaAdded, MediaKind, Pt};
        use str0m::{Event, Rtc};

        let now = std::time::Instant::now();
        let mut browser = Rtc::builder().build(now);
        let mut api = browser.sdp_api();
        let offered_mid = api.add_media(MediaKind::Audio, Direction::RecvOnly, None, None, None);
        let (offer, _pending) = api.apply().expect("audio offer has changes");

        let mut host = super::RtcSession::new(now, 2_000_000);
        let answer = host
            .accept_offer(&offer.to_sdp_string())
            .expect("accept audio offer");

        // The answer answers the offered mid with a send-only Opus track.
        let section = audio_section(&answer);
        assert!(section.contains(&format!("a=mid:{offered_mid}")));
        assert!(section.contains("a=sendonly"));
        let negotiated_pt = opus_payload_type(section).expect("answer has an opus/48000/2 rtpmap");

        // Negotiated state on the host side for that mid.
        let direction = host
            .rtc
            .media(offered_mid)
            .expect("negotiated audio media")
            .direction();
        assert_eq!(direction, Direction::SendOnly);

        let action = host.on_event(Event::MediaAdded(MediaAdded {
            mid: offered_mid,
            kind: MediaKind::Audio,
            direction,
            simulcast: None,
        }));
        assert!(matches!(action, Some(super::RtcAction::AudioReady)));
        assert!(host.audio_mid == Some(offered_mid));
        assert_eq!(host.audio_pt, Some(Pt::new_with_value(negotiated_pt)));

        // ICE has not connected, so the frame is refused and nothing is recorded.
        let frame = crate::audio::AudioFrame {
            seq: 0,
            capture_ts_us: 0,
            discontinuity: true,
            payload: vec![0xfc, 0xff, 0xfe],
        };
        assert!(!host.write_audio(frame).expect("write before ICE"));
    }
}
