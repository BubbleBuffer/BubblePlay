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
use str0m::media::{KeyframeRequest, MediaKind, Mid, Pt};
use str0m::media::MediaTime;
use str0m::{Candidate, Event, Rtc};
use tracing::{debug, info, warn};

/// Events the run loop cares about, extracted from str0m's Output::Event.
pub enum RtcAction {
    IceConnected,
    IceDisconnected,
    /// Browser opened the input data channel.
    ChannelOpen(Vec<u8>), // placeholder not used; channel data below
    /// Binary data from the input channel.
    InputData(Vec<u8>),
    /// PLI/FIR — force an IDR.
    KeyframeRequested,
    /// Egress bandwidth estimate (bps).
    Bwe(u64),
    Closed,
}

pub struct RtcSession {
    pub rtc: Rtc,
    pub video_mid: Option<Mid>,
    pub h264_pt: Option<Pt>,
    pub ice_connected: bool,
    /// Buffered remote candidates that arrived before the offer was accepted.
    pending_remote: Vec<String>,
    /// Base instant for the 90 kHz RTP clock.
    clock_start: Instant,
    frames_written: u64,
}

impl RtcSession {
    pub fn new(now: Instant) -> Self {
        let rtc = Rtc::builder()
            .enable_bwe(Some(str0m::bwe::Bitrate::mbps(20)))
            .build(now);
        Self {
            rtc,
            video_mid: None,
            h264_pt: None,
            ice_connected: false,
            pending_remote: Vec::new(),
            clock_start: now,
            frames_written: 0,
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

        // Drain any remote candidates that trickled in early.
        let buffered = std::mem::take(&mut self.pending_remote);
        for c in buffered {
            self.add_remote_candidate(&c);
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
        if self.video_mid.is_none() && !self.ice_connected {
            // Before an offer is accepted, just buffer.
            self.pending_remote.push(sdp.to_string());
            return;
        }
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
                        warn!(?state, "ICE disconnected");
                        Some(RtcAction::IceDisconnected)
                    }
                    _ => None,
                }
            }
            Event::MediaAdded(added) => {
                if added.kind == MediaKind::Video && added.direction.is_sending() {
                    self.video_mid = Some(added.mid);
                    // Pick the H.264 payload type. Prefer High profile (0x64..),
                    // then anything; NVENC emits High.
                    let mut fallback: Option<Pt> = None;
                    let mut high: Option<Pt> = None;
                    if let Some(writer) = self.rtc.writer(added.mid) {
                        for p in writer.payload_params() {
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
                    self.h264_pt = high.or(fallback);
                    info!(mid = %added.mid, pt = ?self.h264_pt, "video send media added");
                }
                None
            }
            Event::ChannelData(data) => Some(RtcAction::InputData(data.data)),
            Event::KeyframeRequest(KeyframeRequest { kind, .. }) => {
                debug!(?kind, "keyframe request (PLI/FIR)");
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
    /// Silently a no-op before ICE connects or until the video m-line is known
    /// (str0m drops pre-connect writes itself; we guard the pt/mid too).
    pub fn write_video(&mut self, access_unit: Vec<u8>) -> Result<()> {
        let (Some(mid), Some(pt)) = (self.video_mid, self.h264_pt) else {
            return Ok(()); // media not negotiated yet
        };
        let Some(writer) = self.rtc.writer(mid) else {
            return Ok(());
        };
        let elapsed = self.clock_start.elapsed();
        let rtp_time =
            MediaTime::from_90khz(((elapsed.as_secs_f64()) * 90_000.0) as u64);
        writer.write(pt, Instant::now(), rtp_time, access_unit)?;
        self.frames_written += 1;
        Ok(())
    }

    pub fn frames_written(&self) -> u64 {
        self.frames_written
    }

    pub fn alive(&self) -> bool {
        self.rtc.is_alive()
    }
}
