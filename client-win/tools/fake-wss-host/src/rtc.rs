//! The WebRTC answerer behind `--webrtc`. Each `/ws` offer gets one session: a
//! loopback UDP socket, one str0m `Rtc`, and one pump task that owns both. The
//! pump is the only code that touches the socket or the session. The hub
//! sends it commands (a trickled candidate, a drop, or shutdown by closing the
//! channel) and hears back through tagged notices.
//!
//! Video is the recording's access units, paced at the run's fps from the
//! first keyframe on. Audio is the tone's 10 ms Opus frames. Both data
//! channels are read, and each valid cursor-sync (type 8) is answered with a
//! cursor-state packet on the lane it came in on. The poll is bounded: str0m's
//! own deadline, clamped to `MAX_POLL`, so the task never spins.
//!
//! Everything the pump does is counted in its `Report`, including the RTP
//! packets it actually sent, so a test can see that no video went out.

use std::io::ErrorKind;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::{json, Value};
use str0m::change::SdpOffer;
use str0m::channel::ChannelId;
use str0m::format::Codec as RtpCodec;
use str0m::media::{Frequency, MediaKind, MediaTime, Mid, Pt};
use str0m::net::{Protocol, Receive, Transmit};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::{interval, sleep_until, Instant as TokioInstant, MissedTickBehavior};

use crate::audio::{self, ToneEncoder};
use crate::cursor;
use crate::input_record::classify;
use crate::model::Source;
use crate::stream::Codec;

/// The longest the pump sleeps before it polls str0m again, whatever str0m asked for.
const MAX_POLL: Duration = Duration::from_millis(50);
const RECV_BYTES: usize = 65_536;
const VIDEO_CLOCK_HZ: u64 = 90_000;
/// RTP timestamp step of one 10 ms audio frame at 48 kHz.
const AUDIO_STEP: u64 = audio::FRAME_SAMPLES as u64;
/// H.264 High profile, the top byte of `profile_level_id`.
const H264_HIGH: u32 = 0x64;
const LANE_CONTROL: &str = "input-control";
const LANE_MOTION: &str = "input-motion";
pub const COMMAND_DEPTH: usize = 8;
pub const NOTICE_DEPTH: usize = 256;

/// What `--webrtc` turns on. `drop_at` is applied by the hub, not the pump.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Knobs {
    /// Answer the offer but send and receive no UDP.
    pub block_ice: bool,
    /// Channels and audio run, and no video is written.
    pub stall_first_frame: bool,
    /// End the RTC session once, this far into the run.
    pub drop_at: Option<Duration>,
}

/// Everything one session needs from the run.
pub struct Session {
    pub sources: Vec<Arc<dyn Source>>,
    pub fps: u32,
    pub knobs: Knobs,
    /// The browser's offer SDP.
    pub offer: String,
}

#[derive(Debug)]
pub enum Command {
    /// A browser candidate, the `candidate:` string with no `a=` prefix.
    Remote(String),
    /// WSS took over: stop writing video and audio RTP. The peer and both data
    /// channels stay up, as the browser keeps them as a lifecycle anchor.
    StopMedia,
    /// End the session now with a stable reason token. The DTLS close_notify
    /// is sent and drained before the socket closes, so the browser sees `Closed`.
    End(&'static str),
    /// A monitor switch or a stream-mode change: the next recording keyframe
    /// starts the new picture. `generation` is the generation it carries (a
    /// switch); `None` keeps the current one (a mode change). The peer stays up.
    Reseek { generation: Option<u32> },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Notice {
    Answer(String),
    Candidate(String),
    Connected,
    Disconnected,
    ChannelOpen(String),
    /// One binary message from a data channel, valid or not.
    Input {
        channel: String,
        bytes: Vec<u8>,
    },
    /// The first keyframe's first RTP packet left the socket, so the frame is
    /// on the wire. `rtp_timestamp` is read from that packet's header. Only the
    /// fake's own send is reported; presentation is the browser's word.
    FirstKeyframe {
        generation: u32,
        rtp_timestamp: u32,
        codec: &'static str,
    },
    /// The pump has stopped. The reason is a stable token.
    Ended(String),
}

/// The RTC generation of session `id`: stable for the session, nonzero, and
/// different from the next session's. The browser's admission gate compares
/// generations, so a zero here would be ambiguous.
pub fn session_generation(id: u64) -> u32 {
    (id as u32).wrapping_add(1).max(1)
}

/// Notices are tagged with the session id, so the hub ignores a session it has
/// already replaced.
pub type Notices = mpsc::Sender<(u64, Notice)>;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Report {
    /// Why the session stopped: `run-complete`, an `End` reason, or `failed:<stage>`.
    pub ended: String,
    pub error: Option<String>,
    pub video_codec: Option<String>,
    pub video_pt: Option<u8>,
    pub audio_pt: Option<u8>,
    /// Milliseconds from the offer to ICE connected.
    pub connected_ms: Option<f64>,
    /// Milliseconds from the offer to the first keyframe's first RTP packet leaving.
    pub first_keyframe_ms: Option<f64>,
    /// The RTP timestamp in the header of that packet: the admission proof.
    pub admitted_rtp_timestamp: Option<u32>,
    /// WSS took over and video and audio RTP stopped; the peer stayed up.
    pub media_stopped: bool,
    /// The DTLS close_notify left the socket before the session ended.
    pub close_notify_sent: bool,
    pub disconnects: u64,
    pub channels: Vec<String>,
    pub video_frames: u64,
    pub video_frames_suppressed: u64,
    /// Sent datagrams whose RTP header carries the video payload type.
    pub video_rtp_packets: u64,
    pub keyframe_requests: u64,
    pub audio_frames: u64,
    pub audio_frames_not_admitted: u64,
    pub audio_rtp_packets: u64,
    pub inputs: u64,
    pub inputs_invalid: u64,
    pub echoes: u64,
    pub echo_requests: u64,
    pub echo_refused: u64,
    /// From the datagram reaching the pump to its echo being written.
    pub turnaround_us_max: u64,
    pub turnaround_us_total: u64,
    pub blocked_datagrams: u64,
    pub transmit_errors: u64,
    /// First-keyframe admissions on this session, one per keyframe that left
    /// after a start or a `Reseek` (the first keyframe included).
    pub admissions: u64,
    /// Generation carried by the last admission.
    pub admitted_generation: Option<u32>,
    /// `Reseek` commands applied: monitor switches and mode changes.
    pub reseeks: u64,
}

impl Report {
    /// A report for a session that never ran, carrying only why.
    pub fn failed(ended: &str) -> Self {
        Self {
            ended: ended.to_owned(),
            ..Self::default()
        }
    }

    pub fn to_json(&self) -> Value {
        let mean = (self.echoes > 0).then(|| self.turnaround_us_total / self.echoes);
        json!({
            "ended": self.ended,
            "error": self.error,
            "video": {
                "codec": self.video_codec,
                "pt": self.video_pt,
                "frames": self.video_frames,
                "suppressed": self.video_frames_suppressed,
                "rtp_packets": self.video_rtp_packets,
                "first_keyframe_ms": self.first_keyframe_ms,
                "admitted_rtp_timestamp": self.admitted_rtp_timestamp,
                "keyframe_requests": self.keyframe_requests,
            },
            "audio": {
                "pt": self.audio_pt,
                "frames": self.audio_frames,
                "not_admitted": self.audio_frames_not_admitted,
                "rtp_packets": self.audio_rtp_packets,
            },
            "ice": {
                "connected_ms": self.connected_ms,
                "disconnects": self.disconnects,
            },
            "channels": self.channels,
            "input": {
                "messages": self.inputs,
                "invalid": self.inputs_invalid,
                "echoes": self.echoes,
                "echo_requests": self.echo_requests,
                "echo_refused": self.echo_refused,
                "turnaround_us_max": self.turnaround_us_max,
                "turnaround_us_mean": mean,
            },
            "admissions": self.admissions,
            "admitted_generation": self.admitted_generation,
            "reseeks": self.reseeks,
            "media_stopped": self.media_stopped,
            "close_notify_sent": self.close_notify_sent,
            "blocked_datagrams": self.blocked_datagrams,
            "transmit_errors": self.transmit_errors,
        })
    }
}

/// The `webrtc` block of the JSON report.
pub fn summary(reports: &[Report], knobs: Knobs) -> Value {
    json!({
        "knobs": {
            "block_ice": knobs.block_ice,
            "stall_first_frame": knobs.stall_first_frame,
            "rtc_drop_at_s": knobs.drop_at.map(|at| at.as_secs_f64()),
        },
        "sessions": reports.iter().map(Report::to_json).collect::<Vec<_>>(),
    })
}

/// Which video the fake sends. HEVC wins when a HEVC recording exists and the
/// browser negotiated HEVC; otherwise H.264 High, then any H.264, as the host
/// negotiates. `None` when neither can be sent.
pub fn choose_video(
    available: &[Codec],
    hevc: Option<Pt>,
    h264_high: Option<Pt>,
    h264_any: Option<Pt>,
) -> Option<(Codec, Pt)> {
    if available.contains(&Codec::Hevc) {
        if let Some(pt) = hevc {
            return Some((Codec::Hevc, pt));
        }
    }
    if available.contains(&Codec::H264) {
        return h264_high.or(h264_any).map(|pt| (Codec::H264, pt));
    }
    None
}

/// The cursor-state answer to one valid cursor-sync (type 8): the same ack
/// and position, in bounds and visible. `None` for any other packet.
pub fn cursor_echo(data: &[u8]) -> Option<Vec<u8>> {
    match data {
        [8, s0, s1, x0, x1, y0, y1] => Some(
            cursor::Packet::State {
                ack: Some(u16::from_le_bytes([*s0, *s1])),
                x: u16::from_le_bytes([*x0, *x1]),
                y: u16::from_le_bytes([*y0, *y1]),
                in_bounds: true,
                visible: true,
            }
            .encode(),
        ),
        _ => None,
    }
}

/// The wire payload type of a negotiated PT. `Pt` serializes as its number.
fn wire_pt(pt: Pt) -> Option<u8> {
    serde_json::to_value(pt)
        .ok()?
        .as_u64()
        .and_then(|value| u8::try_from(value).ok())
}

struct VideoOut {
    mid: Mid,
    pt: Pt,
    wire_pt: Option<u8>,
    source: Arc<dyn Source>,
    /// The next access unit to write; wraps at the recording's end.
    index: usize,
}

struct AudioOut {
    mid: Mid,
    pt: Pt,
    wire_pt: Option<u8>,
    timestamp: u64,
    started: bool,
}

struct Pump {
    id: u64,
    /// The generation the next admission carries: the session's own until a switch.
    generation: u32,
    /// The next keyframe that leaves is an admission. True until one has left,
    /// and again after a `Reseek`.
    want_admission: bool,
    rtc: Rtc,
    socket: UdpSocket,
    local: SocketAddr,
    knobs: Knobs,
    fps: u32,
    sources: Vec<Arc<dyn Source>>,
    tone: ToneEncoder,
    notices: Notices,
    offered_at: Instant,
    connected: bool,
    /// Open data channels by id, for the two lanes the browser creates.
    channels: Vec<(ChannelId, String)>,
    video: Option<VideoOut>,
    audio: Option<AudioOut>,
    /// When the datagram being handled reached the pump.
    received_at: Instant,
    /// A keyframe was written and its first RTP packet has not left yet.
    keyframe_pending: Option<u32>,
    /// The DTLS close_notify is being drained; the next datagram is it.
    closing: bool,
    report: Report,
}

impl Pump {
    async fn start(session: Session, id: u64, notices: Notices) -> Result<Self> {
        let offered_at = Instant::now();
        let socket = UdpSocket::bind(("127.0.0.1", 0))
            .await
            .context("bind loopback UDP")?;
        let local = socket.local_addr().context("loopback address")?;
        let mut rtc = Rtc::builder().enable_h265(true).build(offered_at);
        let candidate = Candidate::host(local, "udp").context("host candidate")?;
        let candidate_line = candidate.to_sdp_string();
        // The candidate must be added before accept_offer, so the answer carries it.
        rtc.add_local_candidate(candidate);
        let offer = SdpOffer::from_sdp_string(&session.offer).context("parse offer")?;
        let answer = rtc
            .sdp_api()
            .accept_offer(offer)
            .context("accept offer")?
            .to_sdp_string();
        let tone = ToneEncoder::new().context("audio encoder")?;
        let pump = Self {
            id,
            generation: session_generation(id),
            want_admission: true,
            rtc,
            socket,
            local,
            knobs: session.knobs,
            fps: session.fps,
            sources: session.sources,
            tone,
            notices,
            offered_at,
            connected: false,
            channels: Vec::new(),
            video: None,
            audio: None,
            received_at: offered_at,
            keyframe_pending: None,
            closing: false,
            report: Report::default(),
        };
        pump.notify(Notice::Answer(answer)).await;
        pump.notify(Notice::Candidate(candidate_line)).await;
        Ok(pump)
    }

    /// Queues a notice for the hub. The future owns a sender clone, so no borrow
    /// of the pump is held across the await (the pump is not `Sync`). The hub
    /// may be gone during shutdown, and then nobody wants the notice.
    fn notify(&self, notice: Notice) -> impl std::future::Future<Output = ()> + Send {
        let notices = self.notices.clone();
        let id = self.id;
        async move {
            let _ = notices.send((id, notice)).await;
        }
    }

    /// Drain str0m's outputs until it asks for a timeout, and return that deadline.
    async fn flush(&mut self) -> Result<Instant> {
        loop {
            match self.rtc.poll_output().context("poll RTC output")? {
                Output::Timeout(at) => return Ok(at),
                Output::Transmit(transmit) => self.transmit(transmit).await,
                Output::Event(event) => self.on_event(event).await,
            }
        }
    }

    async fn transmit(&mut self, transmit: Transmit) {
        if self.knobs.block_ice {
            self.report.blocked_datagrams += 1;
            return;
        }
        match self
            .socket
            .send_to(&transmit.contents, transmit.destination)
            .await
        {
            Ok(_) => {
                if self.closing {
                    self.report.close_notify_sent = true;
                }
                if let Some(rtp_timestamp) = self.count_rtp(&transmit.contents) {
                    self.admit(rtp_timestamp).await;
                }
            }
            Err(_) => self.report.transmit_errors += 1,
        }
    }

    /// Counts a sent RTP datagram by its payload type. The header is clear
    /// even under SRTP, so this needs no keys. Returns the header's RTP
    /// timestamp when this is the first video packet of a keyframe to leave.
    fn count_rtp(&mut self, bytes: &[u8]) -> Option<u32> {
        if bytes.len() < 12 || bytes[0] >> 6 != 2 {
            return None;
        }
        let pt = bytes[1] & 0x7f;
        if self.video.as_ref().and_then(|v| v.wire_pt) == Some(pt) {
            self.report.video_rtp_packets += 1;
            let timestamp = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
            return take_admission(&mut self.keyframe_pending, timestamp);
        } else if self.audio.as_ref().and_then(|a| a.wire_pt) == Some(pt) {
            self.report.audio_rtp_packets += 1;
        }
        None
    }

    /// The keyframe is on the wire: report it with the timestamp the packet
    /// carries, which is what the browser checks presentation against.
    async fn admit(&mut self, rtp_timestamp: u32) {
        self.want_admission = false;
        self.report.admissions += 1;
        self.report.admitted_rtp_timestamp = Some(rtp_timestamp);
        self.report.admitted_generation = Some(self.generation);
        self.report
            .first_keyframe_ms
            .get_or_insert(self.offered_at.elapsed().as_secs_f64() * 1000.0);
        let codec = self
            .video
            .as_ref()
            .map_or("", |video| video.source.codec().wire_name());
        let notice = Notice::FirstKeyframe {
            generation: self.generation,
            rtp_timestamp,
            codec,
        };
        self.notify(notice).await;
    }

    /// Starts the next picture: the next recording keyframe is written and its
    /// first packet is admitted again, under `generation` when one is given.
    /// Nothing is re-offered and the data channels stay open.
    fn reseek(&mut self, generation: Option<u32>) {
        if let Some(generation) = generation {
            self.generation = generation;
        }
        self.report.reseeks += 1;
        self.want_admission = true;
        self.keyframe_pending = None;
        if let Some(video) = self.video.as_mut() {
            let len = video.source.len().max(1);
            video.index = video.source.next_key_at_or_after(video.index) % len;
        }
    }

    fn receive(&mut self, bytes: &[u8], source: SocketAddr) -> Result<()> {
        if self.knobs.block_ice {
            self.report.blocked_datagrams += 1;
            return Ok(());
        }
        // A datagram that is not STUN, DTLS or RTP is not ours; drop it.
        let Ok(receive) = Receive::new(Protocol::Udp, source, self.local, bytes) else {
            return Ok(());
        };
        let now = Instant::now();
        let input = Input::Receive(now, receive);
        if !self.rtc.accepts(&input) {
            return Ok(());
        }
        self.received_at = now;
        self.rtc.handle_input(input).context("handle datagram")
    }

    async fn on_event(&mut self, event: Event) {
        match event {
            Event::Connected
            | Event::IceConnectionStateChange(IceConnectionState::Connected)
            | Event::IceConnectionStateChange(IceConnectionState::Completed) => {
                self.mark_connected().await
            }
            Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                self.connected = false;
                self.report.disconnects += 1;
                self.notify(Notice::Disconnected).await;
            }
            Event::MediaAdded(added)
                if added.kind == MediaKind::Video && added.direction.is_sending() =>
            {
                self.video_added(added.mid)
            }
            Event::MediaAdded(added)
                if added.kind == MediaKind::Audio && added.direction.is_sending() =>
            {
                self.audio_added(added.mid)
            }
            Event::ChannelOpen(id, label) => {
                if label == LANE_CONTROL || label == LANE_MOTION {
                    self.channels.push((id, label.clone()));
                    self.report.channels.push(label.clone());
                    self.notify(Notice::ChannelOpen(label)).await;
                }
            }
            Event::ChannelClose(id) => self.channels.retain(|(open, _)| *open != id),
            Event::ChannelData(data) => self.channel_data(data.id, data.binary, data.data).await,
            Event::KeyframeRequest(_) => {
                self.report.keyframe_requests += 1;
                if let Some(video) = self.video.as_mut() {
                    video.index = video.source.next_key_at_or_after(video.index);
                }
            }
            _ => {}
        }
    }

    async fn mark_connected(&mut self) {
        if self.connected {
            return;
        }
        self.connected = true;
        if self.report.connected_ms.is_none() {
            self.report.connected_ms = Some(self.offered_at.elapsed().as_secs_f64() * 1000.0);
        }
        self.notify(Notice::Connected).await;
    }

    fn video_added(&mut self, mid: Mid) {
        let (mut hevc, mut high, mut any) = (None, None, None);
        if let Some(writer) = self.rtc.writer(mid) {
            for params in writer.payload_params() {
                match params.spec().codec {
                    RtpCodec::H265 => {
                        hevc.get_or_insert(params.pt());
                    }
                    RtpCodec::H264 => {
                        let profile = params.spec().format.profile_level_id.unwrap_or(0) >> 16;
                        if profile == H264_HIGH {
                            high.get_or_insert(params.pt());
                        } else {
                            any.get_or_insert(params.pt());
                        }
                    }
                    _ => {}
                }
            }
        }
        let available: Vec<Codec> = self.sources.iter().map(|source| source.codec()).collect();
        let Some((codec, pt)) = choose_video(&available, hevc, high, any) else {
            return;
        };
        let Some(source) = self
            .sources
            .iter()
            .find(|source| source.codec() == codec)
            .cloned()
        else {
            return;
        };
        self.report.video_codec = Some(codec.wire_name().to_owned());
        self.report.video_pt = wire_pt(pt);
        self.video = Some(VideoOut {
            mid,
            pt,
            wire_pt: wire_pt(pt),
            source,
            index: 0,
        });
    }

    fn audio_added(&mut self, mid: Mid) {
        let pt = self.rtc.writer(mid).and_then(|writer| {
            writer
                .payload_params()
                .find(|params| params.spec().codec == RtpCodec::Opus)
                .map(|params| params.pt())
        });
        if let Some(pt) = pt {
            self.report.audio_pt = wire_pt(pt);
            self.audio = Some(AudioOut {
                mid,
                pt,
                wire_pt: wire_pt(pt),
                timestamp: 0,
                started: false,
            });
        }
    }

    async fn channel_data(&mut self, id: ChannelId, binary: bool, data: Vec<u8>) {
        let Some(label) = self
            .channels
            .iter()
            .find(|(open, _)| *open == id)
            .map(|(_, label)| label.clone())
        else {
            return;
        };
        self.report.inputs += 1;
        let valid = binary && classify(&data).valid;
        if !valid {
            self.report.inputs_invalid += 1;
        }
        if valid {
            if let Some(packet) = cursor_echo(&data) {
                self.report.echo_requests += 1;
                self.echo(id, &packet);
            }
        }
        self.notify(Notice::Input {
            channel: label,
            bytes: data,
        })
        .await;
    }

    fn echo(&mut self, id: ChannelId, packet: &[u8]) {
        let turnaround = u64::try_from(self.received_at.elapsed().as_micros()).unwrap_or(u64::MAX);
        let Some(mut channel) = self.rtc.channel(id) else {
            self.report.echo_refused += 1;
            return;
        };
        match channel.write(true, packet) {
            Ok(true) => {
                self.report.echoes += 1;
                self.report.turnaround_us_total += turnaround;
                self.report.turnaround_us_max = self.report.turnaround_us_max.max(turnaround);
            }
            _ => self.report.echo_refused += 1,
        }
    }

    /// One frame of video from the recording, when the session can carry it.
    fn video_tick(&mut self) {
        if !self.connected || self.video.is_none() {
            return;
        }
        if self.knobs.stall_first_frame || self.report.media_stopped {
            self.report.video_frames_suppressed += 1;
            return;
        }
        let fps = u64::from(self.fps.max(1));
        let rtp_90k = self.report.video_frames * VIDEO_CLOCK_HZ / fps;
        let Some(video) = self.video.as_mut() else {
            return;
        };
        let index = video.index;
        let key = video.source.is_key(index);
        let payload = video.source.payload(index);
        let Some(writer) = self.rtc.writer(video.mid) else {
            return;
        };
        match writer.write(
            video.pt,
            Instant::now(),
            MediaTime::from_90khz(rtp_90k),
            payload,
        ) {
            Ok(()) => {
                video.index = (index + 1) % video.source.len().max(1);
                self.report.video_frames += 1;
                // Admission waits for the packet itself; see `admit`.
                if key && self.want_admission {
                    // str0m 0.21 packetizes this MediaTime directly at 90 kHz.
                    // Older buffered packets may leave first after a reseek;
                    // only this written IDR's timestamp proves the transition.
                    self.keyframe_pending = Some(rtp_90k as u32);
                }
            }
            Err(_) => self.report.transmit_errors += 1,
        }
    }

    /// One 10 ms tone frame. The encoder runs whenever audio is negotiated, so
    /// the tone stays continuous even while nothing is admitted.
    async fn audio_tick(&mut self) {
        if self.audio.is_none() || self.report.media_stopped {
            return;
        }
        let payload = match self.tone.next_payload() {
            Ok(payload) => payload,
            Err(error) => {
                self.report
                    .error
                    .get_or_insert(format!("tone encoder: {error:#}"));
                return;
            }
        };
        let connected = self.connected;
        let Some(audio) = self.audio.as_mut() else {
            return;
        };
        if !connected {
            self.report.audio_frames_not_admitted += 1;
            return;
        }
        let Some(writer) = self.rtc.writer(audio.mid) else {
            self.report.audio_frames_not_admitted += 1;
            return;
        };
        let talkspurt = !audio.started;
        let written = writer.start_of_talkspurt(talkspurt).write(
            audio.pt,
            Instant::now(),
            MediaTime::new(audio.timestamp, Frequency::FORTY_EIGHT_KHZ),
            payload,
        );
        match written {
            Ok(()) => {
                audio.started = true;
                audio.timestamp += AUDIO_STEP;
                self.report.audio_frames += 1;
            }
            Err(_) => self.report.audio_frames_not_admitted += 1,
        }
    }

    fn remote_candidate(&mut self, candidate: &str) {
        // A string that does not parse is ignored, as the browser's own
        // end-of-candidates marker would be.
        if let Ok(candidate) = Candidate::from_sdp_string(candidate) {
            self.rtc.add_remote_candidate(candidate);
        }
    }

    /// Sends the DTLS close_notify and drains it before the socket is dropped,
    /// so the browser's `Rtc` reports `Closed` promptly. A session that never
    /// reached DTLS has nothing to close; str0m then just stops, which is fine.
    async fn close_dtls(&mut self) {
        if self.rtc.close().is_err() {
            return;
        }
        self.closing = true;
        let _ = self.flush().await;
    }

    /// The pump's loop. Returns the stop reason once the session is over.
    async fn serve(&mut self, commands: &mut mpsc::Receiver<Command>) -> String {
        let fps = self.fps.max(1);
        let mut frames = interval(Duration::from_secs_f64(1.0 / f64::from(fps)));
        frames.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut audio_ticks = interval(audio::FRAME_PERIOD);
        // An Opus packet represents exactly 480 samples (10 ms). Skipping a
        // late Windows timer tick slowed this RTP clock below 48 kHz and made
        // the receiver conceal and then reject packets that arrived late.
        // Catch up the packet cadence; flush runs before every single tick,
        // keeping the RTC output queue drained during catch-up.
        audio_ticks.set_missed_tick_behavior(MissedTickBehavior::Burst);
        let mut buf = vec![0u8; RECV_BYTES];
        loop {
            let deadline = match self.flush().await {
                Ok(deadline) => deadline,
                Err(error) => {
                    self.report.error = Some(format!("{error:#}"));
                    return "failed:rtc".into();
                }
            };
            let wake = deadline.min(Instant::now() + MAX_POLL);
            tokio::select! {
                received = self.socket.recv_from(&mut buf) => match received {
                    Ok((len, source)) => {
                        if let Err(error) = self.receive(&buf[..len], source) {
                            self.report.error = Some(format!("{error:#}"));
                            return "failed:rtc".into();
                        }
                    }
                    // Windows reports an earlier send's ICMP refusal here; the next recv is fine.
                    Err(error) if error.kind() == ErrorKind::ConnectionReset => {
                        self.report.transmit_errors += 1;
                    }
                    Err(error) => {
                        self.report.error = Some(format!("socket receive: {error}"));
                        return "failed:socket".into();
                    }
                },
                _ = sleep_until(TokioInstant::from_std(wake)) => {
                    if let Err(error) = self.rtc.handle_input(Input::Timeout(Instant::now())) {
                        self.report.error = Some(format!("timeout: {error}"));
                        return "failed:rtc".into();
                    }
                }
                _ = frames.tick() => self.video_tick(),
                _ = audio_ticks.tick() => self.audio_tick().await,
                command = commands.recv() => match command {
                    Some(Command::Remote(candidate)) => self.remote_candidate(&candidate),
                    Some(Command::StopMedia) => self.report.media_stopped = true,
                    Some(Command::Reseek { generation }) => self.reseek(generation),
                    Some(Command::End(reason)) => return reason.into(),
                    None => return "run-complete".into(),
                },
            }
        }
    }
}

/// Runs one session to its end and returns what it did. The task is joined by
/// the hub; closing `commands` (or a `Drop`) is how it is asked to stop.
pub async fn run(
    session: Session,
    id: u64,
    mut commands: mpsc::Receiver<Command>,
    notices: Notices,
) -> Report {
    let mut pump = match Pump::start(session, id, notices.clone()).await {
        Ok(pump) => pump,
        Err(error) => {
            let ended = "failed:setup";
            let _ = notices.send((id, Notice::Ended(ended.into()))).await;
            return Report {
                error: Some(format!("{error:#}")),
                ..Report::failed(ended)
            };
        }
    };
    let ended = pump.serve(&mut commands).await;
    if !ended.starts_with("failed:") {
        pump.close_dtls().await;
    }
    pump.rtc.disconnect();
    pump.report.ended = ended.clone();
    pump.notify(Notice::Ended(ended)).await;
    pump.report
}

/// Admit only the written IDR, even when an older packet was already queued.
fn take_admission(pending: &mut Option<u32>, timestamp: u32) -> Option<u32> {
    if *pending == Some(timestamp) {
        pending.take()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queued_old_video_cannot_admit_a_new_idr_and_each_idr_admits_once() {
        // Includes RTP wrap: the buffered packet belongs to the previous picture.
        let mut pending = Some(1500);
        for old in [u32::MAX - 100, 0, 1499] {
            assert_eq!(take_admission(&mut pending, old), None);
            assert_eq!(pending, Some(1500));
        }
        assert_eq!(take_admission(&mut pending, 1500), Some(1500));
        assert_eq!(take_admission(&mut pending, 1500), None);
        assert_eq!(pending, None);
    }

    fn pt(value: u8) -> Option<Pt> {
        Some(Pt::new_with_value(value))
    }

    #[test]
    fn hevc_is_chosen_only_when_recorded_and_negotiated() {
        let both = [Codec::H264, Codec::Hevc];
        assert_eq!(
            choose_video(&both, pt(98), pt(96), None),
            Some((Codec::Hevc, Pt::new_with_value(98)))
        );
        // Negotiated but not recorded: H.264.
        assert_eq!(
            choose_video(&[Codec::H264], pt(98), pt(96), None),
            Some((Codec::H264, Pt::new_with_value(96)))
        );
        // Recorded but not negotiated: H.264.
        assert_eq!(
            choose_video(&both, None, pt(96), None),
            Some((Codec::H264, Pt::new_with_value(96)))
        );
        // Recorded HEVC only, browser negotiated H.264 only: nothing to send.
        assert_eq!(choose_video(&[Codec::Hevc], None, pt(96), None), None);
    }

    #[test]
    fn h264_prefers_high_then_any() {
        assert_eq!(
            choose_video(&[Codec::H264], None, pt(100), pt(96)),
            Some((Codec::H264, Pt::new_with_value(100)))
        );
        assert_eq!(
            choose_video(&[Codec::H264], None, None, pt(96)),
            Some((Codec::H264, Pt::new_with_value(96)))
        );
        assert_eq!(choose_video(&[Codec::H264], None, None, None), None);
        assert_eq!(choose_video(&[], pt(98), pt(96), pt(97)), None);
    }

    #[test]
    fn cursor_sync_is_answered_with_its_ack_and_position() {
        let echo = cursor_echo(&[8, 0x34, 0x12, 10, 0, 0x20, 0x01]).expect("echo");
        assert_eq!(echo, vec![128, 0x34, 0x12, 10, 0, 0x20, 0x01, 7]);
        assert_eq!(
            cursor_echo(&[8, 1, 0, 0, 0, 0]),
            None,
            "short is not echoed"
        );
        assert_eq!(cursor_echo(&[9, 1, 0, 0, 0, 0, 0, 0]), None, "only type 8");
    }

    #[test]
    fn session_generation_is_nonzero_and_per_session() {
        assert_eq!(session_generation(0), 1);
        assert_eq!(session_generation(4), 5);
        assert_eq!(
            session_generation(u64::from(u32::MAX)),
            1,
            "wraps past zero"
        );
        assert_ne!(session_generation(7), session_generation(8));
    }

    #[test]
    fn summary_names_the_knobs_and_every_session() {
        let knobs = Knobs {
            block_ice: true,
            stall_first_frame: false,
            drop_at: Some(Duration::from_millis(1500)),
        };
        let value = summary(&[Report::failed("rtc-dropped")], knobs);
        assert_eq!(value["knobs"]["block_ice"], true);
        assert_eq!(value["knobs"]["rtc_drop_at_s"], 1.5);
        assert_eq!(value["sessions"][0]["ended"], "rtc-dropped");
        assert!(value["sessions"][0]["input"]["turnaround_us_mean"].is_null());
    }
}
