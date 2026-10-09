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
use str0m::media::MediaTime;
use str0m::media::{KeyframeRequest, MediaKind, Mid, Pt};
use str0m::{Candidate, Event, Rtc};
use tracing::{debug, info, warn};

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
    Closed,
}

pub struct RtcSession {
    pub rtc: Rtc,
    pub video_mid: Option<Mid>,
    pub video_pt: Option<Pt>,
    pub video_codec: Option<VideoCodec>,
    pub ice_connected: bool,
    /// ICE reported Disconnected; a later Connected is a recovery.
    ice_lost: bool,
    /// Base instant for the 90 kHz RTP clock.
    clock_start: Instant,
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
            ice_connected: false,
            ice_lost: false,
            clock_start: now,
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
    /// Returns `true` only when the access unit was actually admitted to the
    /// negotiated video writer. This keeps transport telemetry honest during
    /// connection setup and teardown.
    pub fn write_video(&mut self, access_unit: Vec<u8>) -> Result<bool> {
        let (Some(mid), Some(pt)) = (self.video_mid, self.video_pt) else {
            return Ok(false); // media not negotiated yet
        };
        let Some(writer) = self.rtc.writer(mid) else {
            return Ok(false);
        };
        let elapsed = self.clock_start.elapsed();
        let rtp_time = MediaTime::from_90khz(((elapsed.as_secs_f64()) * 90_000.0) as u64);
        writer.write(pt, Instant::now(), rtp_time, access_unit)?;
        self.frames_written += 1;
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

#[cfg(test)]
mod tests {
    use super::preferred_negotiated_codec;
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
}
