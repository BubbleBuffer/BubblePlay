//! The RTC transport pump.  This is deliberately the only place that owns
//! str0m and the media UDP socket; the D3D/NVENC producer only owns an AU
//! sender, so a slow network cannot extend encoder latency.

use std::collections::VecDeque;
use std::net::{SocketAddr, UdpSocket};
use std::os::windows::io::AsRawSocket;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use str0m::net::{Protocol, Receive};
use str0m::{Input, Output};
use tracing::{debug, info, warn};

use crate::activity::Activity;
use crate::audio::AudioFrame;
use crate::rtc::{RtcAction, RtcSession};
use crate::stun;

pub const AU_QUEUE_BOUND: usize = 1;
/// Five 10 ms frames: at most 50 ms of PC audio waits behind a stalled pump.
const AUDIO_QUEUE_BOUND: usize = 5;
const MAPPING_REFRESH_INTERVAL: Duration = Duration::from_secs(20);
/// How long a media-carrying RTC peer may sit in ICE Disconnected before the
/// session gives up on it. Short Wi-Fi hiccups recover inside this window.
pub const RTC_DISCONNECT_GRACE: Duration = Duration::from_secs(5);

/// What the session loop does after an RTC peer lifecycle change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RtcPeerDecision {
    /// Keep the session and the peer; wait for ICE to come back.
    Continue,
    /// WSS carries the session: stop only the RTC transport.
    EndPeer,
    /// The RTC peer was the session's media path and it is gone.
    EndSession,
}

/// Tracks whether the RTC peer is healthy, in its disconnect grace, or ended.
/// The RTC peer ends the session only while it carries media; once WSS is the
/// active transport, losing the peer tears down just the peer.
#[derive(Debug, Default)]
pub struct RtcPeerLifecycle {
    disconnected_since: Option<Instant>,
    ended: bool,
}

impl RtcPeerLifecycle {
    pub fn ended(&self) -> bool {
        self.ended
    }

    pub fn on_connected(&mut self) {
        self.disconnected_since = None;
    }

    /// ICE reported Disconnected; this can still recover.
    pub fn on_disconnected(&mut self, now: Instant, wss_active: bool) -> RtcPeerDecision {
        if self.ended {
            return RtcPeerDecision::Continue;
        }
        if wss_active {
            return self.end_peer();
        }
        self.disconnected_since.get_or_insert(now);
        RtcPeerDecision::Continue
    }

    /// The RTC epoch closed or its transport thread stopped; it cannot recover.
    pub fn on_closed(&mut self, wss_active: bool) -> RtcPeerDecision {
        if self.ended {
            return RtcPeerDecision::Continue;
        }
        if wss_active {
            return self.end_peer();
        }
        RtcPeerDecision::EndSession
    }

    /// Checked every loop: the grace ends the peer (WSS) or the session (RTC).
    pub fn poll(&mut self, now: Instant, wss_active: bool) -> RtcPeerDecision {
        let Some(since) = self.disconnected_since else {
            return RtcPeerDecision::Continue;
        };
        if self.ended {
            return RtcPeerDecision::Continue;
        }
        if wss_active {
            return self.end_peer();
        }
        if now.duration_since(since) >= RTC_DISCONNECT_GRACE {
            return RtcPeerDecision::EndSession;
        }
        RtcPeerDecision::Continue
    }

    fn end_peer(&mut self) -> RtcPeerDecision {
        self.ended = true;
        self.disconnected_since = None;
        RtcPeerDecision::EndPeer
    }
}

#[derive(Default)]
struct TransportCounters {
    tx_packets: u64,
    tx_bytes: u64,
    admitted_frames: u64,
    admitted_bytes: u64,
    cursor_feedback_sent: u64,
    audio_frames_written: u64,
    audio_frames_not_admitted: u64,
}

impl TransportCounters {
    fn has_media_activity(&self, producer_skipped: u64, au_age_count: u64) -> bool {
        self.tx_packets != 0
            || self.admitted_frames != 0
            || self.audio_frames_written != 0
            || self.cursor_feedback_sent != 0
            || producer_skipped != 0
            || au_age_count != 0
    }
}

/// PC audio waiting for the RTP audio track. It is separate from the video
/// slot. When full it drops the oldest frame, so latency stays bounded.
#[derive(Default)]
struct AudioQueue {
    frames: VecDeque<AudioFrame>,
    dropped: u64,
}

impl AudioQueue {
    fn push(&mut self, frame: AudioFrame) {
        if self.frames.len() >= AUDIO_QUEUE_BOUND {
            self.frames.pop_front();
            self.dropped += 1;
        }
        self.frames.push_back(frame);
    }

    fn pop(&mut self) -> Option<AudioFrame> {
        self.frames.pop_front()
    }

    /// Called when the peer is replaced or reset, so a new peer never plays
    /// sound captured for the old one.
    fn clear(&mut self) {
        self.frames.clear();
    }

    fn take_dropped(&mut self) -> u64 {
        std::mem::take(&mut self.dropped)
    }
}

pub struct AccessUnit {
    pub id: u64,
    pub data: Vec<u8>,
    pub produced_at: Instant,
    pub is_idr: bool,
}

pub enum TransportCommand {
    Offer(String),
    RemoteCandidate(String),
    CursorStateReliable([u8; 8]),
    /// End the current peer while retaining the transport socket and thread.
    ResetPeer,
    /// Background DNS resolution completed for NAT mapping maintenance.
    StunServer(SocketAddr),
    Shutdown,
}

pub enum TransportEvent {
    Answer(String),
    OfferRejected,
    Action(RtcAction),
    AccessUnitWritten {
        id: u64,
        is_idr: bool,
        /// 32-bit RTP timestamp the video track used for this access unit.
        rtp_timestamp: u32,
    },
    AccessUnitNotAdmitted {
        id: u64,
        is_idr: bool,
    },
    /// All previous peer events precede this reset acknowledgement.
    PeerReset,
    Stopped,
}

pub struct TransportPump {
    au_sender: AccessUnitSender,
    // Signaling and lifecycle messages are low-rate correctness traffic. They
    // must never be discarded because the RTP pacer is temporarily busy.
    command_tx: Sender<TransportCommand>,
    event_rx: Receiver<TransportEvent>,
    event_activity: Arc<Mutex<Option<Activity>>>,
    cursor_latest: Arc<Mutex<Option<[u8; 8]>>>,
    cursor_feedback_coalesced: Arc<AtomicU64>,
    cursor_kind_latest: Arc<Mutex<ReliableCursorKind>>,
    audio_queue: Arc<Mutex<AudioQueue>>,
    stop: Arc<AtomicBool>,
    wake: Activity,
    join: Option<JoinHandle<()>>,
}

#[derive(Clone)]
pub struct AccessUnitSender {
    tx: SyncSender<AccessUnit>,
    reserved_or_queued: Arc<AtomicBool>,
    skipped_before_encode: Arc<AtomicU64>,
    wake: Activity,
}

pub struct AccessUnitReservation {
    sender: AccessUnitSender,
    active: bool,
}

impl AccessUnitSender {
    /// Reserve the single AU position before touching D3D/NVENC.  The guard
    /// releases the admission on drop if capture or encode fails.
    pub fn reserve(&self) -> Option<AccessUnitReservation> {
        match self.reserved_or_queued.compare_exchange(
            false,
            true,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => Some(AccessUnitReservation {
                sender: self.clone(),
                active: true,
            }),
            Err(_) => {
                self.skipped_before_encode.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }
}

impl AccessUnitReservation {
    pub fn submit(mut self, au: AccessUnit) -> bool {
        let admitted = try_admit_access_unit(&self.sender.tx, au);
        if !admitted {
            self.sender
                .reserved_or_queued
                .store(false, Ordering::Release);
        }
        self.active = false;
        if admitted {
            self.sender.wake.notify();
        }
        admitted
    }
}

impl Drop for AccessUnitReservation {
    fn drop(&mut self) {
        if self.active {
            self.sender
                .reserved_or_queued
                .store(false, Ordering::Release);
        }
    }
}

impl TransportPump {
    pub fn spawn(
        socket: UdpSocket,
        host_addr: Option<SocketAddr>,
        mapped_addr: Option<SocketAddr>,
        initial_bitrate: u32,
    ) -> Self {
        let wait = Activity::new().expect("create RTC wake socket");
        let wake = wait.clone();
        let (au_tx, au_rx) = mpsc::sync_channel(AU_QUEUE_BOUND);
        let au_sender = AccessUnitSender {
            tx: au_tx,
            reserved_or_queued: Arc::new(AtomicBool::new(false)),
            skipped_before_encode: Arc::new(AtomicU64::new(0)),
            wake: wake.clone(),
        };
        let transport_gate = au_sender.reserved_or_queued.clone();
        let transport_skips = au_sender.skipped_before_encode.clone();
        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let event_activity = Arc::new(Mutex::new(None));
        let event_tx = EventSender {
            tx: event_tx,
            activity: event_activity.clone(),
        };
        let cursor_latest = Arc::new(Mutex::new(None));
        let cursor_feedback_coalesced = Arc::new(AtomicU64::new(0));
        let transport_cursor_latest = cursor_latest.clone();
        let transport_cursor_coalesced = cursor_feedback_coalesced.clone();
        let cursor_kind_latest = Arc::new(Mutex::new(ReliableCursorKind::default()));
        let transport_cursor_kind = cursor_kind_latest.clone();
        let audio_queue = Arc::new(Mutex::new(AudioQueue::default()));
        let transport_audio = audio_queue.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let join = thread::Builder::new()
            .name("bubbleplay-rtc".into())
            .spawn(move || {
                if let Err(error) = transport_loop(
                    socket,
                    host_addr,
                    mapped_addr,
                    initial_bitrate,
                    au_rx,
                    transport_gate,
                    transport_skips,
                    command_rx,
                    event_tx.clone(),
                    thread_stop,
                    transport_cursor_latest,
                    transport_cursor_coalesced,
                    transport_cursor_kind,
                    transport_audio,
                    wait,
                ) {
                    warn!(?error, "RTC transport pump stopped with error");
                }
                let _ = event_tx.send(TransportEvent::Stopped);
            })
            .expect("spawn RTC transport pump");
        Self {
            au_sender,
            command_tx,
            event_rx,
            event_activity,
            cursor_latest,
            cursor_feedback_coalesced,
            cursor_kind_latest,
            audio_queue,
            stop,
            wake,
            join: Some(join),
        }
    }

    /// This is called before scaling/encoding.  A full slot means the previous
    /// reference frame is still waiting, so the producer must skip this frame.
    pub fn reserve_access_unit(&self) -> Option<AccessUnitReservation> {
        self.au_sender.reserve()
    }

    /// Queue one PC-audio frame for the audio track and wake the pump. Frames
    /// that are not admitted (no negotiated Opus track, or ICE not connected)
    /// are discarded by the pump. Video is unaffected.
    pub fn submit_audio(&self, frame: AudioFrame) {
        push_audio(&self.audio_queue, &self.wake, frame);
    }

    /// Audio submission for a thread that must not depend on the video
    /// producer. The sink performs the same bounded push and wake as
    /// `submit_audio`, and discards frames once the pump has been shut down.
    pub fn audio_sink(&self) -> Arc<dyn Fn(AudioFrame) + Send + Sync> {
        let queue = self.audio_queue.clone();
        let wake = self.wake.clone();
        let stop = self.stop.clone();
        Arc::new(move |frame| {
            if stop.load(Ordering::Acquire) {
                return;
            }
            push_audio(&queue, &wake, frame);
        })
    }
    pub fn command(&self, command: TransportCommand) -> bool {
        let sent = try_send_command(&self.command_tx, command);
        if sent {
            self.wake.notify();
        }
        sent
    }

    /// Resolve outside the pump; every Binding uses the persistent media socket.
    pub fn set_stun_server(&self, server: &str) -> Result<()> {
        let server = server.to_owned();
        let commands = self.command_tx.clone();
        let wake = self.wake.clone();
        thread::Builder::new()
            .name("bubbleplay-stun-resolve".into())
            .spawn(move || {
                use std::net::ToSocketAddrs;
                match server
                    .to_socket_addrs()
                    .map(|mut addresses| addresses.find(SocketAddr::is_ipv4))
                {
                    Ok(Some(address)) => {
                        if commands.send(TransportCommand::StunServer(address)).is_ok() {
                            wake.notify();
                        }
                    }
                    Ok(None) => warn!("STUN refresh DNS returned no IPv4 address"),
                    Err(error) => warn!(?error, "STUN refresh DNS failed"),
                }
            })
            .context("spawn STUN refresh resolution")?;
        Ok(())
    }
    /// Publish routine cursor telemetry without ever building a queue. The RTC
    /// thread consumes the newest state on its next bounded transport wake.
    pub fn publish_cursor_state(&self, state: [u8; 8]) {
        replace_latest_cursor(&self.cursor_latest, state, &self.cursor_feedback_coalesced);
        self.wake.notify();
    }
    /// Reliable type changes use a separate latest-only slot. Keep the last
    /// delivered kind too, so a fresh RTC offer can replay it when control opens.
    pub fn publish_cursor_kind(&self, kind: [u8; 2]) {
        self.cursor_kind_latest
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .publish(kind);
        self.wake.notify();
    }
    /// Attach the producer wake before waiting for transport events.
    pub fn set_activity(&self, activity: Activity) {
        *self
            .event_activity
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(activity.clone());
        activity.notify();
    }

    pub fn try_event(&self) -> Option<TransportEvent> {
        self.event_rx.try_recv().ok()
    }

    pub fn shutdown(mut self) {
        // Shutdown is out-of-band so a signaling backlog cannot delay join.
        self.stop.store(true, Ordering::Release);
        self.wake.notify();
        drop(self.au_sender);
        drop(self.command_tx);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Bounded oldest-drop push shared by `submit_audio` and `audio_sink`.
fn push_audio(queue: &Mutex<AudioQueue>, wake: &Activity, frame: AudioFrame) {
    queue
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(frame);
    wake.notify();
}

/// Non-blocking bounded admission. `false` is the producer's explicit
/// skip-before-encode signal; it never displaces a queued access unit.
pub fn try_admit_access_unit<T>(tx: &SyncSender<T>, value: T) -> bool {
    match tx.try_send(value) {
        Ok(()) => true,
        Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => false,
    }
}

fn try_send_command<T>(tx: &Sender<T>, command: T) -> bool {
    tx.send(command).is_ok()
}

fn replace_latest_cursor(slot: &Mutex<Option<[u8; 8]>>, state: [u8; 8], coalesced: &AtomicU64) {
    let mut pending = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if pending.replace(state).is_some() {
        coalesced.fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Default)]
struct ReliableCursorKind {
    current: Option<[u8; 2]>,
    pending: bool,
}

impl ReliableCursorKind {
    fn publish(&mut self, kind: [u8; 2]) {
        self.current = Some(kind);
        self.pending = true;
    }
    fn replay(&mut self) {
        self.pending = self.current.is_some();
    }
    fn next(&self) -> Option<[u8; 2]> {
        if self.pending {
            self.current
        } else {
            None
        }
    }
    fn delivered(&mut self, kind: [u8; 2]) {
        if self.current == Some(kind) {
            self.pending = false;
        }
    }
}
fn stop_takes_priority(stop_requested: bool) -> bool {
    stop_requested
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeadEpochAction {
    Continue,
    RenewIdle,
    StopNegotiated,
}

struct MappingRefresh {
    server: SocketAddr,
    transaction: Option<[u8; 12]>,
    next_request: Instant,
}

impl MappingRefresh {
    fn new(server: SocketAddr, now: Instant) -> Self {
        Self {
            server,
            transaction: None,
            next_request: now,
        }
    }

    fn tick(&mut self, socket: &UdpSocket, now: Instant) {
        if now < self.next_request {
            return;
        }
        let (request, transaction) = stun::binding_request();
        self.next_request = now + MAPPING_REFRESH_INTERVAL;
        match socket.send_to(&request, self.server) {
            Ok(_) => self.transaction = Some(transaction),
            Err(error) => debug!(?error, "STUN mapping refresh send failed"),
        }
    }

    fn response(&mut self, bytes: &[u8], source: SocketAddr) -> Option<SocketAddr> {
        if source != self.server {
            return None;
        }
        let mapped = stun::parse_binding_response(bytes, self.transaction.as_ref()?)?;
        self.transaction = None;
        Some(mapped)
    }
}

fn dead_epoch_action(alive: bool, offer_active: bool) -> DeadEpochAction {
    match (alive, offer_active) {
        (true, _) => DeadEpochAction::Continue,
        (false, false) => DeadEpochAction::RenewIdle,
        (false, true) => DeadEpochAction::StopNegotiated,
    }
}

fn schedule_timeout(deadline: Instant, now: Instant, offer_active: bool) -> Instant {
    if deadline > now {
        return deadline;
    }
    now + if offer_active {
        // An active pacer uses an immediate deadline to release queued RTP.
        // Adding the UDP polling ceiling here throttles every packet burst.
        Duration::ZERO
    } else {
        Duration::from_secs(1)
    }
}

#[derive(Clone)]
struct EventSender {
    tx: Sender<TransportEvent>,
    activity: Arc<Mutex<Option<Activity>>>,
}

impl EventSender {
    fn send(
        &self,
        event: TransportEvent,
    ) -> std::result::Result<(), mpsc::SendError<TransportEvent>> {
        let result = self.tx.send(event);
        if let Some(activity) = self
            .activity
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
        {
            activity.notify();
        }
        result
    }
}

fn emit(tx: &EventSender, event: TransportEvent) {
    // This is an unbounded MPSC hand-off: it never blocks the pacer and a
    // critical offer/answer/ICE/input/admission event cannot vanish on a full
    // bounded queue. Receiver disconnection is terminal and observed by the
    // producer through Stopped/command failure during shutdown.
    if tx.send(event).is_err() {
        debug!("transport event receiver disconnected");
    }
}

/// `host_addr` is `None` when local IP discovery failed: the RTC session then
/// has no candidates (WebRTC cannot connect) but the pump keeps running so
/// the WSS session is unaffected.
fn fresh_epoch(
    socket: &UdpSocket,
    host_addr: Option<SocketAddr>,
    mapped: Option<SocketAddr>,
    bitrate: u32,
) -> Result<RtcSession> {
    let mut rtc = RtcSession::new(Instant::now(), bitrate);
    let Some(host_addr) = host_addr else {
        return Ok(rtc);
    };
    rtc.add_host_candidate(host_addr)?;
    drain_outputs(&mut rtc, socket, None, None)?;
    if let Some(mapped) = mapped {
        rtc.add_srflx_candidate(mapped, host_addr)?;
        // str0m requires every accepted mutation to be followed by its own
        // complete output drain; do not batch candidate mutations.
        drain_outputs(&mut rtc, socket, None, None)?;
    }
    Ok(rtc)
}

fn drain_outputs(
    rtc: &mut RtcSession,
    socket: &UdpSocket,
    events: Option<&EventSender>,
    mut counters: Option<&mut TransportCounters>,
) -> Result<Instant> {
    loop {
        match rtc.rtc.poll_output()? {
            Output::Transmit(t) => {
                if socket.send_to(&t.contents, t.destination).is_ok() {
                    if let Some(counters) = counters.as_deref_mut() {
                        counters.tx_packets += 1;
                        counters.tx_bytes =
                            counters.tx_bytes.saturating_add(t.contents.len() as u64);
                    }
                }
            }
            Output::Event(event) => {
                if let Some(action) = rtc.on_event(event) {
                    if let Some(tx) = events {
                        emit(tx, TransportEvent::Action(action));
                    }
                }
            }
            Output::Timeout(deadline) => return Ok(deadline),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn transport_loop(
    socket: UdpSocket,
    host_addr: Option<SocketAddr>,
    mut mapped: Option<SocketAddr>,
    bitrate: u32,
    au_rx: Receiver<AccessUnit>,
    au_gate: Arc<AtomicBool>,
    skipped_before_encode: Arc<AtomicU64>,
    command_rx: Receiver<TransportCommand>,
    events: EventSender,
    stop: Arc<AtomicBool>,
    cursor_latest: Arc<Mutex<Option<[u8; 8]>>>,
    cursor_feedback_coalesced: Arc<AtomicU64>,
    cursor_kind_latest: Arc<Mutex<ReliableCursorKind>>,
    audio_queue: Arc<Mutex<AudioQueue>>,
    wait: Activity,
) -> Result<()> {
    socket
        .set_nonblocking(true)
        .context("make media socket nonblocking")?;
    let mut offer_active = false;
    let mut mapping_refresh: Option<MappingRefresh> = None;
    let mut rtc = fresh_epoch(&socket, host_addr, mapped, bitrate)?;
    let mut timeout = schedule_timeout(
        drain_outputs(&mut rtc, &socket, Some(&events), None)?,
        Instant::now(),
        offer_active,
    );
    let mut buffer = vec![0; 2000];
    let mut running = true;
    let mut next_log = Instant::now() + Duration::from_secs(1);
    let mut au_age_sum_us = 0u64;
    let mut au_age_count = 0u64;
    let mut au_age_max_us = 0u64;
    let mut deadline_late_count = 0u64;
    let mut deadline_late_max_us = 0u64;
    let mut counters = TransportCounters::default();
    let mut reliable_cursor_pending: Option<[u8; 8]> = None;
    while running {
        if stop_takes_priority(stop.load(Ordering::Acquire)) {
            break;
        }
        // Exactly one accepted mutation is followed by a full drain below.
        let mut mutated = false;
        match command_rx.try_recv() {
            Ok(_) if stop_takes_priority(stop.load(Ordering::Acquire)) => break,
            Ok(TransportCommand::Offer(sdp)) => {
                if let Some(refresh) = &mut mapping_refresh {
                    refresh.next_request = Instant::now();
                }
                audio_queue
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clear();
                rtc = fresh_epoch(&socket, host_addr, mapped, bitrate)?;
                cursor_kind_latest
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .replay();
                reliable_cursor_pending = None;
                cursor_latest
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take();
                match rtc.accept_offer(&sdp) {
                    Ok(answer) => {
                        offer_active = true;
                        emit(&events, TransportEvent::Answer(answer));
                    }
                    Err(e) => {
                        offer_active = false;
                        warn!(?e, "rejecting SDP offer");
                        emit(&events, TransportEvent::OfferRejected);
                    }
                }
                mutated = true;
            }
            Ok(TransportCommand::ResetPeer) => {
                offer_active = false;
                reliable_cursor_pending = None;
                cursor_latest
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take();
                while let Ok(au) = au_rx.try_recv() {
                    emit(
                        &events,
                        TransportEvent::AccessUnitNotAdmitted {
                            id: au.id,
                            is_idr: au.is_idr,
                        },
                    );
                }
                au_gate.store(false, Ordering::Release);
                audio_queue
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clear();
                rtc = fresh_epoch(&socket, host_addr, mapped, bitrate)?;
                emit(&events, TransportEvent::PeerReset);
                mutated = true;
            }
            Ok(TransportCommand::RemoteCandidate(candidate)) => {
                rtc.add_remote_candidate(&candidate);
                mutated = true;
            }
            Ok(TransportCommand::StunServer(server)) => {
                mapping_refresh = Some(MappingRefresh::new(server, Instant::now()));
            }
            Ok(TransportCommand::CursorStateReliable(state)) => {
                reliable_cursor_pending = Some(state)
            }
            Ok(TransportCommand::Shutdown) | Err(mpsc::TryRecvError::Disconnected) => {
                running = false
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
        if !running {
            break;
        }
        if let Some(refresh) = &mut mapping_refresh {
            refresh.tick(&socket, Instant::now());
        }
        match dead_epoch_action(rtc.alive(), offer_active) {
            DeadEpochAction::Continue => {}
            DeadEpochAction::RenewIdle => {
                info!("renewing expired idle RTC epoch");
                rtc = fresh_epoch(&socket, host_addr, mapped, bitrate)?;
                timeout = schedule_timeout(
                    drain_outputs(&mut rtc, &socket, Some(&events), Some(&mut counters))?,
                    Instant::now(),
                    offer_active,
                );
                continue;
            }
            DeadEpochAction::StopNegotiated => {
                emit(&events, TransportEvent::Action(RtcAction::Closed));
                offer_active = false;
                continue;
            }
        }
        if !mutated {
            let kind = cursor_kind_latest
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .next();
            if let Some(kind) = kind {
                match rtc.write_cursor_state(true, &kind) {
                    Ok(true) => {
                        cursor_kind_latest
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .delivered(kind);
                        counters.cursor_feedback_sent += 1;
                        mutated = true;
                    }
                    Ok(false) => {}
                    Err(error) => warn!(?error, "write reliable cursor kind"),
                }
            }
        }
        if !mutated {
            if let Some(state) = reliable_cursor_pending {
                match rtc.write_cursor_state(true, &state) {
                    Ok(true) => {
                        reliable_cursor_pending = None;
                        counters.cursor_feedback_sent += 1;
                        mutated = true;
                    }
                    Ok(false) => {}
                    Err(error) => {
                        warn!(?error, "write reliable cursor state");
                        reliable_cursor_pending = None;
                    }
                }
            }
        }
        if !mutated {
            let latest = cursor_latest
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
            if let Some(state) = latest {
                match rtc.write_cursor_state(false, &state) {
                    Ok(true) => {
                        counters.cursor_feedback_sent += 1;
                        mutated = true;
                    }
                    Ok(false) => {
                        let mut pending = cursor_latest
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        if pending.is_none() {
                            *pending = Some(state);
                        }
                    }
                    Err(error) => warn!(?error, "write cursor state"),
                }
            }
        }
        if !mutated {
            if let Ok(au) = au_rx.try_recv() {
                let age_us = Instant::now().duration_since(au.produced_at).as_micros() as u64;
                au_age_sum_us = au_age_sum_us.saturating_add(age_us);
                au_age_count += 1;
                au_age_max_us = au_age_max_us.max(age_us);
                let au_bytes = au.data.len() as u64;
                match rtc.write_video(au.data) {
                    Ok(Some(rtp_timestamp)) => {
                        counters.admitted_frames += 1;
                        counters.admitted_bytes = counters.admitted_bytes.saturating_add(au_bytes);
                        debug!(au_age_us = age_us, "RTC access unit written");
                        emit(
                            &events,
                            TransportEvent::AccessUnitWritten {
                                id: au.id,
                                is_idr: au.is_idr,
                                rtp_timestamp,
                            },
                        );
                    }
                    Ok(None) => emit(
                        &events,
                        TransportEvent::AccessUnitNotAdmitted {
                            id: au.id,
                            is_idr: au.is_idr,
                        },
                    ),
                    Err(e) => {
                        warn!(?e, "write access unit");
                        emit(
                            &events,
                            TransportEvent::AccessUnitNotAdmitted {
                                id: au.id,
                                is_idr: au.is_idr,
                            },
                        );
                    }
                }
                // Keep the reservation held until the producer can observe
                // the corresponding event and schedule IDR recovery.
                au_gate.store(false, Ordering::Release);
                mutated = true;
            }
        }
        if !mutated {
            let frame = audio_queue
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .pop();
            if let Some(frame) = frame {
                // Only an accepted write is a mutation that needs a drain; a
                // frame dropped before negotiation or ICE leaves the peer untouched.
                match rtc.write_audio(frame) {
                    Ok(true) => {
                        counters.audio_frames_written += 1;
                        mutated = true;
                    }
                    Ok(false) => counters.audio_frames_not_admitted += 1,
                    Err(error) => {
                        warn!(?error, "write audio frame");
                        counters.audio_frames_not_admitted += 1;
                        mutated = true;
                    }
                }
            }
        }
        if !mutated {
            match socket.recv_from(&mut buffer) {
                Ok((n, source)) => {
                    if let Some(address) = mapping_refresh
                        .as_mut()
                        .and_then(|refresh| refresh.response(&buffer[..n], source))
                    {
                        if mapped != Some(address) {
                            info!(%address, "refreshed STUN mapping for next RTC offer");
                            mapped = Some(address);
                        }
                        continue;
                    }
                    // Without a host address there are no candidates, so
                    // nothing inbound can belong to the RTC session.
                    if let (Ok(contents), Some(destination)) = (buffer[..n].try_into(), host_addr) {
                        let input = Input::Receive(
                            Instant::now(),
                            Receive {
                                proto: Protocol::Udp,
                                source,
                                destination,
                                contents,
                            },
                        );
                        if rtc.rtc.accepts(&input) {
                            rtc.rtc.handle_input(input).context("RTC UDP input")?;
                            mutated = true;
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => warn!(?e, "media UDP receive"),
            }
        }
        let now = Instant::now();
        if !mutated && offer_active && now >= timeout {
            let late = now.duration_since(timeout).as_micros() as u64;
            if late != 0 {
                deadline_late_count += 1;
                deadline_late_max_us = deadline_late_max_us.max(late);
            }
            rtc.rtc
                .handle_input(Input::Timeout(now))
                .context("RTC timeout")?;
            mutated = true;
        }
        if now >= next_log {
            let producer_skipped = skipped_before_encode.swap(0, Ordering::Relaxed);
            let cursor_coalesced = cursor_feedback_coalesced.swap(0, Ordering::Relaxed);
            let audio_dropped = audio_queue
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take_dropped();
            if counters.has_media_activity(producer_skipped, au_age_count) {
                info!(
                    tx_packets = counters.tx_packets,
                    tx_bytes = counters.tx_bytes,
                    tx_mbps = (counters.tx_bytes as f64 * 8.0) / 1_000_000.0,
                    admitted_frames = counters.admitted_frames,
                    admitted_bytes = counters.admitted_bytes,
                    admitted_mbps = (counters.admitted_bytes as f64 * 8.0) / 1_000_000.0,
                    admitted_fps = counters.admitted_frames,
                    au_queue_depth = u8::from(au_gate.load(Ordering::Acquire)),
                    au_queue_bound = AU_QUEUE_BOUND,
                    producer_skipped_before_encode = producer_skipped,
                    cursor_feedback_sent = counters.cursor_feedback_sent,
                    cursor_feedback_coalesced = cursor_coalesced,
                    audio_frames_written = counters.audio_frames_written,
                    audio_frames_not_admitted = counters.audio_frames_not_admitted,
                    audio_frames_dropped = audio_dropped,
                    au_age_us_sum = au_age_sum_us,
                    au_age_count,
                    au_age_us_max = au_age_max_us,
                    deadline_late_count,
                    deadline_late_us_max = deadline_late_max_us,
                    "RTC transport"
                );
            } else {
                debug!(
                    cursor_feedback_coalesced = cursor_coalesced,
                    audio_frames_dropped = audio_dropped,
                    deadline_late_count,
                    deadline_late_us_max = deadline_late_max_us,
                    "RTC transport idle"
                );
            }
            next_log = now + Duration::from_secs(1);
            au_age_sum_us = 0;
            au_age_count = 0;
            au_age_max_us = 0;
            deadline_late_count = 0;
            deadline_late_max_us = 0;
            counters = TransportCounters::default();
        }
        if mutated {
            timeout = schedule_timeout(
                drain_outputs(&mut rtc, &socket, Some(&events), Some(&mut counters))?,
                Instant::now(),
                offer_active,
            );
            continue;
        }
        // Media datagrams and every producer/control publication wake this
        // wait immediately. An idle pump has no RTC deadlines to service.
        let mut deadline = if offer_active {
            timeout.min(next_log)
        } else {
            next_log
        };
        if let Some(refresh) = &mapping_refresh {
            deadline = deadline.min(refresh.next_request);
        }
        let sockets = if offer_active || mapping_refresh.is_some() {
            vec![(socket.as_raw_socket(), false)]
        } else {
            vec![]
        };
        wait.wait(deadline.saturating_duration_since(Instant::now()), &sockets)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{AudioQueue, AUDIO_QUEUE_BOUND};
    use crate::audio::AudioFrame;

    fn audio_frame(seq: u32) -> AudioFrame {
        AudioFrame {
            seq,
            capture_ts_us: u64::from(seq) * 10_000,
            discontinuity: seq == 0,
            payload: vec![0xfc, 0xff, 0xfe],
        }
    }

    #[test]
    fn audio_queue_drops_oldest_frame_and_clears_on_reset() {
        let mut queue = AudioQueue::default();
        for seq in 0..(AUDIO_QUEUE_BOUND as u32 + 2) {
            queue.push(audio_frame(seq));
        }
        // The two oldest frames were dropped; the newest AUDIO_QUEUE_BOUND remain in order.
        assert_eq!(queue.take_dropped(), 2);
        assert_eq!(queue.take_dropped(), 0);
        let remaining: Vec<u32> = std::iter::from_fn(|| queue.pop())
            .map(|frame| frame.seq)
            .collect();
        assert_eq!(remaining, vec![2, 3, 4, 5, 6]);

        // A peer reset discards pending audio rather than sending it to the new peer.
        queue.push(audio_frame(7));
        queue.clear();
        assert!(queue.pop().is_none());
    }

    #[test]
    fn mapping_refresh_uses_media_socket_and_validates_server_and_transaction() {
        let media = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let server = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let now = Instant::now();
        let mut refresh = super::MappingRefresh::new(server.local_addr().unwrap(), now);
        refresh.tick(&media, now);
        let mut request = [0u8; 20];
        let (size, source) = server.recv_from(&mut request).unwrap();
        assert_eq!(size, 20);
        assert_eq!(source, media.local_addr().unwrap());
        let mut response = [0u8; 32];
        response[..20].copy_from_slice(&request);
        response[..2].copy_from_slice(&0x0101u16.to_be_bytes());
        response[2..4].copy_from_slice(&12u16.to_be_bytes());
        response[20..22].copy_from_slice(&0x0020u16.to_be_bytes());
        response[22..24].copy_from_slice(&8u16.to_be_bytes());
        response[25] = 1;
        response[26..28].copy_from_slice(&(4444u16 ^ 0x2112).to_be_bytes());
        for (index, byte) in [203u8, 0, 113, 9].into_iter().enumerate() {
            response[28 + index] = byte ^ request[4 + index];
        }
        assert_eq!(
            refresh.response(&response, media.local_addr().unwrap()),
            None
        );
        let mut wrong_transaction = response;
        wrong_transaction[8] ^= 1;
        assert_eq!(
            refresh.response(&wrong_transaction, server.local_addr().unwrap()),
            None
        );
        assert_eq!(
            refresh.response(&response, server.local_addr().unwrap()),
            Some("203.0.113.9:4444".parse().unwrap())
        );
        assert_eq!(
            refresh.response(&response, server.local_addr().unwrap()),
            None
        );
        server.set_nonblocking(true).unwrap();
        refresh.tick(&media, now + Duration::from_secs(1));
        assert_eq!(
            server.recv_from(&mut request).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        refresh.tick(&media, now + super::MAPPING_REFRESH_INTERVAL);
        assert_eq!(server.recv_from(&mut request).unwrap().0, 20);
    }

    #[test]
    fn resetting_a_peer_keeps_the_pump_alive_and_wakes_the_producer() {
        let activity = crate::activity::Activity::new().unwrap();
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let pump = super::TransportPump::spawn(socket, None, None, 2_000_000);
        pump.set_activity(activity.clone());
        for _ in 0..2 {
            assert!(pump.command(super::TransportCommand::ResetPeer));
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                match pump.try_event() {
                    Some(super::TransportEvent::PeerReset) => break,
                    Some(super::TransportEvent::Stopped) => panic!("reset stopped reusable pump"),
                    Some(_) => {}
                    None => {
                        assert!(Instant::now() < deadline, "reset acknowledgement timed out");
                        activity
                            .wait(deadline.saturating_duration_since(Instant::now()), &[])
                            .unwrap();
                    }
                }
            }
        }
        pump.shutdown();
    }

    use super::{
        dead_epoch_action, emit, replace_latest_cursor, schedule_timeout, stop_takes_priority,
        try_admit_access_unit, try_send_command, DeadEpochAction, ReliableCursorKind,
        RtcPeerDecision, RtcPeerLifecycle, TransportCounters, TransportEvent, RTC_DISCONNECT_GRACE,
    };

    #[test]
    fn reliable_cursor_kind_survives_control_not_ready_and_replays_on_reconnect() {
        let mut slot = ReliableCursorKind::default();
        assert_eq!(slot.next(), None);
        slot.publish([129, 1]);
        // A channel that is not open does not acknowledge delivery.
        assert_eq!(slot.next(), Some([129, 1]));
        assert_eq!(slot.next(), Some([129, 1]));
        slot.publish([129, 2]);
        slot.delivered([129, 1]); // In-flight old data cannot clear a newer kind.
        assert_eq!(slot.next(), Some([129, 2]));
        slot.delivered([129, 2]);
        assert_eq!(slot.next(), None);
        slot.replay();
        assert_eq!(slot.next(), Some([129, 2]));
        slot.delivered([129, 2]);
        slot.publish([129, 2]); // Explicit sync replays even an unchanged kind.
        assert_eq!(slot.next(), Some([129, 2]));
    }
    use std::sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{channel, sync_channel},
        Mutex,
    };
    use std::time::{Duration, Instant};

    #[test]
    fn rtc_epoch_without_a_host_address_starts_without_candidates() {
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        assert!(super::fresh_epoch(&socket, None, None, 2_000_000).is_ok());
    }

    #[test]
    fn rtc_peer_loss_under_wss_ends_only_the_peer() {
        let now = Instant::now();
        let mut peer = RtcPeerLifecycle::default();
        assert_eq!(peer.on_closed(true), RtcPeerDecision::EndPeer);
        assert!(peer.ended());
        // Later RTC events (Stopped after the shutdown) are ignored.
        assert_eq!(peer.on_closed(true), RtcPeerDecision::Continue);
        assert_eq!(peer.on_disconnected(now, true), RtcPeerDecision::Continue);

        let mut peer = RtcPeerLifecycle::default();
        assert_eq!(peer.on_disconnected(now, true), RtcPeerDecision::EndPeer);
    }

    #[test]
    fn media_carrying_rtc_peer_gets_a_disconnect_grace() {
        let now = Instant::now();
        let mut peer = RtcPeerLifecycle::default();
        assert_eq!(peer.on_disconnected(now, false), RtcPeerDecision::Continue);
        assert_eq!(
            peer.poll(now + Duration::from_millis(4_900), false),
            RtcPeerDecision::Continue
        );
        peer.on_connected();
        assert_eq!(
            peer.poll(now + Duration::from_secs(60), false),
            RtcPeerDecision::Continue
        );

        assert_eq!(peer.on_disconnected(now, false), RtcPeerDecision::Continue);
        assert_eq!(
            peer.poll(now + RTC_DISCONNECT_GRACE, false),
            RtcPeerDecision::EndSession
        );
        assert_eq!(peer.on_closed(false), RtcPeerDecision::EndSession);
    }

    #[test]
    fn a_switch_to_wss_during_the_grace_ends_only_the_peer() {
        let now = Instant::now();
        let mut peer = RtcPeerLifecycle::default();
        assert_eq!(peer.on_disconnected(now, false), RtcPeerDecision::Continue);
        assert_eq!(
            peer.poll(now + Duration::from_secs(1), true),
            RtcPeerDecision::EndPeer
        );
        assert!(peer.ended());
    }

    #[test]
    fn full_access_unit_slot_rejects_before_encode_and_preserves_the_first_frame() {
        let (tx, rx) = sync_channel(1);
        assert!(try_admit_access_unit(&tx, 1));
        assert!(!try_admit_access_unit(&tx, 2));
        assert_eq!(rx.recv().unwrap(), 1);
    }

    #[test]
    fn lifecycle_events_survive_a_busy_consumer_without_blocking_the_sender() {
        let (tx, rx) = channel();
        let tx = super::EventSender {
            tx,
            activity: std::sync::Arc::new(Mutex::new(None)),
        };
        for _ in 0..1_000 {
            emit(&tx, TransportEvent::Stopped);
        }
        assert_eq!(rx.try_iter().count(), 1_000);
    }

    #[test]
    fn disconnected_transport_command_is_rejected() {
        let (tx, rx) = channel::<()>();
        drop(rx);
        assert!(!try_send_command(&tx, ()));
    }

    #[test]
    fn cursor_feedback_slot_keeps_only_the_latest_state() {
        let slot = Mutex::new(None);
        let coalesced = AtomicU64::new(0);
        replace_latest_cursor(&slot, [1; 8], &coalesced);
        replace_latest_cursor(&slot, [2; 8], &coalesced);
        assert_eq!(*slot.lock().unwrap(), Some([2; 8]));
        assert_eq!(coalesced.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn idle_transport_windows_do_not_emit_info_level_media_stats() {
        let idle = TransportCounters::default();
        assert!(!idle.has_media_activity(0, 0));

        let active = TransportCounters {
            admitted_frames: 1,
            ..TransportCounters::default()
        };
        assert!(active.has_media_activity(0, 0));
    }

    #[test]
    fn stop_request_takes_priority_over_queued_commands() {
        assert!(stop_takes_priority(true));
        assert!(!stop_takes_priority(false));
    }

    #[test]
    fn expired_idle_epoch_is_renewed_but_expired_negotiated_epoch_stops() {
        assert_eq!(dead_epoch_action(true, false), DeadEpochAction::Continue);
        assert_eq!(dead_epoch_action(false, false), DeadEpochAction::RenewIdle);
        assert_eq!(
            dead_epoch_action(false, true),
            DeadEpochAction::StopNegotiated
        );
    }

    #[test]
    fn idle_deadlines_back_off_but_due_media_deadlines_run_immediately() {
        let now = std::time::Instant::now();
        let future = now + std::time::Duration::from_millis(17);
        assert_eq!(schedule_timeout(future, now, false), future);
        assert_eq!(
            schedule_timeout(now, now, false),
            now + std::time::Duration::from_secs(1)
        );
        assert_eq!(schedule_timeout(now, now, true), now);
        assert_eq!(
            schedule_timeout(now - std::time::Duration::from_millis(1), now, true),
            now
        );
    }
}
