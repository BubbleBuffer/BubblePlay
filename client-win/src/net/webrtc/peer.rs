use super::{auto::Startup, ice, offer};
use crate::net::{NetError, TransportChoice};
use serde_json::Value;
use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};
use str0m::{
    change::{SdpAnswer, SdpPendingOffer},
    channel::ChannelId,
    format::Codec,
    media::{KeyframeRequestKind, Mid, Pt},
    net::{Protocol, Receive},
    Candidate, Event, Input, Output, Rtc,
};
use tokio::net::UdpSocket;

struct Probe {
    server: SocketAddr,
    tid: [u8; 12],
    attempts: u8,
    next: Instant,
    done: bool,
}

pub struct Peer {
    pub rtc: Rtc,
    pub socket: Arc<UdpSocket>,
    pub startup: Startup,
    pub video_mid: Mid,
    pub audio_mid: Mid,
    pub video_codecs: HashMap<Pt, crate::net::Codec>,
    pub audio_pt: Option<Pt>,
    pub control: Option<ChannelId>,
    pub motion: Option<ChannelId>,
    pub offer_at: Instant,
    pub offer: String,
    pending_offer: Option<SdpPendingOffer>,
    pending_ice: Vec<String>,
    pub local_candidates: Vec<Candidate>,
    pub remote_candidates: Vec<Candidate>,
    destination: SocketAddr,
    loopback_only: bool,
    deadline: Instant,
    probes: Vec<Probe>,
    resolvers: Vec<tokio::task::JoinHandle<Vec<SocketAddr>>>,
    pub trickle: Vec<String>,
    pub stun_requests: u64,
    pub srflx_count: u64,
}
fn error(e: impl std::fmt::Display) -> NetError {
    NetError::Socket(e.to_string())
}
impl Peer {
    pub async fn new(
        choice: TransportChoice,
        checking_timeout: Duration,
        hevc: bool,
        servers: &Value,
        loopback_only: bool,
        route_ip: Option<IpAddr>,
    ) -> Result<Self, NetError> {
        let ip = if loopback_only {
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        } else if let Some(ip) = route_ip.filter(IpAddr::is_ipv4) {
            ip
        } else {
            // Bind a concrete interface so Receive.destination is the actual
            // address, as required by str0m. Additional addresses are candidates
            // only when backed by a socket; never advertise unsendable routes.
            if_addrs::get_if_addrs()
                .map_err(error)?
                .into_iter()
                .map(|i| i.ip())
                .find(|ip| ip.is_ipv4() && !ip.is_loopback())
                .ok_or_else(|| error("no IPv4 interface for WebRTC"))?
        };
        let socket = Arc::new(
            UdpSocket::bind(SocketAddr::new(ip, 0))
                .await
                .map_err(error)?,
        );
        let socket = Arc::new({
            let std_socket = Arc::try_unwrap(socket)
                .expect("new socket has one owner")
                .into_std()
                .map_err(error)?;
            socket2::SockRef::from(&std_socket)
                .set_recv_buffer_size(4 * 1024 * 1024)
                .map_err(error)?;
            UdpSocket::from_std(std_socket).map_err(error)?
        });
        let destination = socket.local_addr().map_err(error)?;
        let now = Instant::now();
        let mut rtc = offer::rtc(now, hevc);
        let candidate = Candidate::host(destination, "udp").map_err(error)?;
        rtc.add_local_candidate(candidate.clone());
        let (offer, pending_offer, video_mid, audio_mid) = offer::create(&mut rtc);
        // DNS gathering is concurrent with offer/ICE, never a startup gate.
        let resolvers = ice::stun_urls(servers)
            .into_iter()
            .take(16)
            .map(|server| tokio::spawn(async move { ice::resolve(&server, loopback_only).await }))
            .collect();
        Ok(Self {
            rtc,
            socket,
            startup: Startup::new(choice, checking_timeout, now),
            video_mid,
            audio_mid,
            video_codecs: HashMap::new(),
            audio_pt: None,
            control: None,
            motion: None,
            offer_at: now,
            offer,
            pending_offer: Some(pending_offer),
            pending_ice: Vec::new(),
            local_candidates: vec![candidate.clone()],
            remote_candidates: Vec::new(),
            destination,
            loopback_only,
            deadline: now,
            probes: Vec::new(),
            resolvers,
            trickle: vec![candidate.to_sdp_string()],
            stun_requests: 0,
            srflx_count: 0,
        })
    }
    pub fn offer_sent(&mut self, at: Instant) {
        self.offer_at = at;
        self.startup.offer_sent(at);
    }
    pub fn answer(&mut self, sdp: &str) -> Result<(), NetError> {
        if sdp.len() > 256 * 1024 {
            return Err(NetError::Protocol("oversized SDP answer".into()));
        }
        // Answers commonly embed host candidates without trickling them again.
        // Retain their types for pair stats and validate the local-run fence
        // before str0m can install them from the SDP.
        for line in sdp.lines().filter(|line| line.starts_with("a=candidate:")) {
            let candidate = Candidate::from_sdp_string(line.trim_start_matches("a="))
                .map_err(|_| NetError::Protocol("invalid SDP answer candidate".into()))?;
            if self.loopback_only && !candidate.addr().ip().is_loopback() {
                return Err(NetError::Protocol(
                    "non-loopback candidate in SDP answer".into(),
                ));
            }
            if self.remote_candidates.len() < 128 {
                self.remote_candidates.push(candidate);
            }
        }
        let Some(pending) = self.pending_offer.take() else {
            return Ok(());
        };
        let answer = SdpAnswer::from_sdp_string(sdp).map_err(error)?;
        self.rtc
            .sdp_api()
            .accept_answer(pending, answer)
            .map_err(error)?;
        self.startup.progress(Instant::now());
        if let Some(media) = self.rtc.media(self.video_mid) {
            for p in self
                .rtc
                .codec_config()
                .params()
                .iter()
                .filter(|p| media.remote_pts().contains(&p.pt()))
            {
                let codec = match p.spec().codec {
                    Codec::H265 => crate::net::Codec::Hevc,
                    Codec::H264 => crate::net::Codec::H264,
                    _ => continue,
                };
                self.video_codecs.insert(p.pt(), codec);
            }
        }
        self.audio_pt = self.rtc.media(self.audio_mid).and_then(|m| {
            self.rtc
                .codec_config()
                .params()
                .iter()
                .find(|p| p.spec().codec == Codec::Opus && m.remote_pts().contains(&p.pt()))
                .map(|p| p.pt())
        });
        let queued = std::mem::take(&mut self.pending_ice);
        for candidate in queued {
            self.candidate(&candidate)?;
        }
        Ok(())
    }
    pub fn candidate(&mut self, text: &str) -> Result<(), NetError> {
        if text.len() > 2048 {
            return Err(NetError::Protocol("oversized ICE candidate".into()));
        }
        if self.pending_offer.is_some() {
            if self.pending_ice.len() < 128 {
                self.pending_ice.push(text.into());
            }
            return Ok(());
        }
        let candidate = Candidate::from_sdp_string(text.trim_start_matches("a="))
            .map_err(|_| NetError::Protocol("invalid trickled ICE candidate".into()))?;
        if self.loopback_only && !candidate.addr().ip().is_loopback() {
            return Err(NetError::Protocol(
                "non-loopback ICE candidate in local qualification".into(),
            ));
        }
        if self.remote_candidates.len() < 128 {
            self.remote_candidates.push(candidate.clone());
            self.rtc.add_remote_candidate(candidate);
        }
        Ok(())
    }
    pub fn datagram(&mut self, bytes: &[u8], source: SocketAddr) -> Result<bool, NetError> {
        if self.loopback_only && !source.ip().is_loopback() {
            return Ok(false);
        }
        for probe in &mut self.probes {
            if !probe.done && source == probe.server {
                if let Some(mapped) = ice::mapped_response(bytes, &probe.tid) {
                    probe.done = true;
                    if mapped.is_ipv4() && (!self.loopback_only || mapped.ip().is_loopback()) {
                        let candidate =
                            Candidate::server_reflexive(mapped, self.destination, "udp")
                                .map_err(error)?;
                        self.rtc.add_local_candidate(candidate.clone());
                        self.trickle.push(candidate.to_sdp_string());
                        self.local_candidates.push(candidate);
                        self.srflx_count += 1;
                    }
                    return Ok(true);
                }
            }
        }
        let Ok(receive) = Receive::new(Protocol::Udp, source, self.destination, bytes) else {
            return Ok(false);
        };
        let input = Input::Receive(Instant::now(), receive);
        if !self.rtc.accepts(&input) {
            return Ok(false);
        }
        self.rtc.handle_input(input).map_err(error)?;
        Ok(true)
    }
    pub async fn pump(&mut self) -> Result<Vec<Event>, NetError> {
        if !self.rtc.is_alive() {
            return Ok(Vec::new());
        }
        let now = Instant::now();
        for index in (0..self.resolvers.len()).rev() {
            if self.resolvers[index].is_finished() {
                let resolver = self.resolvers.swap_remove(index);
                for server in resolver.await.unwrap_or_default().into_iter().take(1) {
                    self.probes.push(Probe {
                        server,
                        tid: rand::random(),
                        attempts: 0,
                        next: now,
                        done: false,
                    });
                }
            }
        }
        for probe in &mut self.probes {
            if !probe.done && now >= probe.next && probe.attempts < 3 {
                self.socket
                    .send_to(&ice::binding_request(probe.tid), probe.server)
                    .await
                    .map_err(error)?;
                probe.attempts += 1;
                probe.next = now + Duration::from_millis(900);
                self.stun_requests += 1;
            }
        }
        if now >= self.deadline && self.rtc.is_alive() {
            self.rtc.handle_input(Input::Timeout(now)).map_err(error)?;
        }
        let mut events = Vec::new();
        for _ in 0..1024 {
            match self.rtc.poll_output().map_err(error)? {
                Output::Timeout(at) => {
                    self.deadline = at;
                    break;
                }
                Output::Transmit(tx) => {
                    if !self.loopback_only || tx.destination.ip().is_loopback() {
                        self.socket
                            .send_to(&tx.contents, tx.destination)
                            .await
                            .map_err(error)?;
                    }
                }
                Output::Event(event) => events.push(event),
            }
        }
        Ok(events)
    }
    pub fn wait(&self, now: Instant) -> Duration {
        self.deadline
            .saturating_duration_since(now)
            .min(Duration::from_millis(25))
            .max(Duration::from_millis(1))
    }
    pub fn lanes_ready(&self) -> bool {
        self.control.is_some() && self.motion.is_some()
    }
    pub fn send_input(&mut self, packet: &[u8], motion: bool) -> Result<bool, NetError> {
        let id = if motion { self.motion } else { self.control };
        let Some(mut channel) = id.and_then(|id| self.rtc.channel(id)) else {
            return Ok(false);
        };
        let limit = if motion { 64 } else { 262_144 };
        if channel.buffered_amount() > limit {
            return Ok(false);
        }
        channel.write(true, packet).map_err(error)
    }
    pub fn keyframe(&mut self) -> bool {
        if let Some(stream) = self.rtc.direct_api().stream_rx_by_mid(self.video_mid, None) {
            stream.request_keyframe(KeyframeRequestKind::Pli);
            return true;
        }
        false
    }
    pub fn pair_type(&self, local: SocketAddr, remote: SocketAddr) -> String {
        let typ = |list: &[Candidate], addr| {
            list.iter()
                .find(|c| c.addr() == addr)
                .map(|c| match c.kind() {
                    str0m::CandidateKind::Host => "host",
                    str0m::CandidateKind::ServerReflexive => "srflx",
                    str0m::CandidateKind::PeerReflexive => "prflx",
                    str0m::CandidateKind::Relayed => "relay",
                })
                .unwrap_or("prflx")
        };
        format!(
            "{}->{}",
            typ(&self.local_candidates, local),
            typ(&self.remote_candidates, remote)
        )
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        for resolver in &self.resolvers {
            resolver.abort();
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use str0m::change::SdpOffer;
    #[tokio::test]
    async fn embedded_answer_candidate_negotiates_and_records_type_without_trickle() {
        let mut peer = Peer::new(
            TransportChoice::WebRtc,
            crate::net::AUTO_ICE_CHECKING_TIMEOUT,
            false,
            &Value::Null,
            true,
            None,
        )
        .await
        .unwrap();
        let mut answerer = Rtc::new(Instant::now());
        let remote: SocketAddr = "127.0.0.1:43210".parse().unwrap();
        answerer.add_local_candidate(Candidate::host(remote, "udp").unwrap());
        let answer = answerer
            .sdp_api()
            .accept_offer(SdpOffer::from_sdp_string(&peer.offer).unwrap())
            .unwrap()
            .to_sdp_string();
        assert!(answer.contains("a=candidate:"));
        peer.answer(&answer).unwrap();
        assert!(!peer.video_codecs.is_empty());
        assert!(peer.audio_pt.is_some());
        assert_eq!(peer.pair_type(peer.destination, remote), "host->host");
        assert!(peer.pending_offer.is_none());
    }
}
