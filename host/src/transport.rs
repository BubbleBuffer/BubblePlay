//! The RTC transport pump.  This is deliberately the only place that owns
//! str0m and the media UDP socket; the D3D/NVENC producer only owns an AU
//! sender, so a slow network cannot extend encoder latency.

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use str0m::net::{Protocol, Receive};
use str0m::{Input, Output};
use tracing::{debug, info, warn};

use crate::rtc::{RtcAction, RtcSession};

pub const AU_QUEUE_BOUND: usize = 1;
const UDP_POLL_CEILING: Duration = Duration::from_millis(4);

#[derive(Default)]
struct TransportCounters {
    tx_packets: u64,
    tx_bytes: u64,
    admitted_frames: u64,
    admitted_bytes: u64,
    cursor_feedback_sent: u64,
}

impl TransportCounters {
    fn has_media_activity(&self, producer_skipped: u64, au_age_count: u64) -> bool {
        self.tx_packets != 0
            || self.admitted_frames != 0
            || self.cursor_feedback_sent != 0
            || producer_skipped != 0
            || au_age_count != 0
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
    Shutdown,
}

pub enum TransportEvent {
    Answer(String),
    OfferRejected,
    Action(RtcAction),
    AccessUnitWritten { id: u64, is_idr: bool },
    AccessUnitNotAdmitted { id: u64, is_idr: bool },
    Stopped,
}

pub struct TransportPump {
    au_sender: AccessUnitSender,
    // Signaling and lifecycle messages are low-rate correctness traffic. They
    // must never be discarded because the RTP pacer is temporarily busy.
    command_tx: Sender<TransportCommand>,
    event_rx: Receiver<TransportEvent>,
    cursor_latest: Arc<Mutex<Option<[u8; 8]>>>,
    cursor_feedback_coalesced: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

#[derive(Clone)]
pub struct AccessUnitSender {
    tx: SyncSender<AccessUnit>,
    reserved_or_queued: Arc<AtomicBool>,
    skipped_before_encode: Arc<AtomicU64>,
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
        let (au_tx, au_rx) = mpsc::sync_channel(AU_QUEUE_BOUND);
        let au_sender = AccessUnitSender {
            tx: au_tx,
            reserved_or_queued: Arc::new(AtomicBool::new(false)),
            skipped_before_encode: Arc::new(AtomicU64::new(0)),
        };
        let transport_gate = au_sender.reserved_or_queued.clone();
        let transport_skips = au_sender.skipped_before_encode.clone();
        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let cursor_latest = Arc::new(Mutex::new(None));
        let cursor_feedback_coalesced = Arc::new(AtomicU64::new(0));
        let transport_cursor_latest = cursor_latest.clone();
        let transport_cursor_coalesced = cursor_feedback_coalesced.clone();
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
            cursor_latest,
            cursor_feedback_coalesced,
            stop,
            join: Some(join),
        }
    }

    /// This is called before scaling/encoding.  A full slot means the previous
    /// reference frame is still waiting, so the producer must skip this frame.
    pub fn reserve_access_unit(&self) -> Option<AccessUnitReservation> {
        self.au_sender.reserve()
    }
    pub fn command(&self, command: TransportCommand) -> bool {
        try_send_command(&self.command_tx, command)
    }
    /// Publish routine cursor telemetry without ever building a queue. The RTC
    /// thread consumes the newest state on its next bounded transport wake.
    pub fn publish_cursor_state(&self, state: [u8; 8]) {
        replace_latest_cursor(&self.cursor_latest, state, &self.cursor_feedback_coalesced);
    }
    pub fn try_event(&self) -> Option<TransportEvent> {
        self.event_rx.try_recv().ok()
    }

    pub fn shutdown(mut self) {
        // Shutdown is out-of-band so a signaling backlog cannot delay join.
        self.stop.store(true, Ordering::Release);
        drop(self.au_sender);
        drop(self.command_tx);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
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

fn stop_takes_priority(stop_requested: bool) -> bool {
    stop_requested
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeadEpochAction {
    Continue,
    RenewIdle,
    StopNegotiated,
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

fn emit(tx: &Sender<TransportEvent>, event: TransportEvent) {
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
    events: Option<&Sender<TransportEvent>>,
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

fn transport_loop(
    socket: UdpSocket,
    host_addr: Option<SocketAddr>,
    mapped: Option<SocketAddr>,
    bitrate: u32,
    au_rx: Receiver<AccessUnit>,
    au_gate: Arc<AtomicBool>,
    skipped_before_encode: Arc<AtomicU64>,
    command_rx: Receiver<TransportCommand>,
    events: Sender<TransportEvent>,
    stop: Arc<AtomicBool>,
    cursor_latest: Arc<Mutex<Option<[u8; 8]>>>,
    cursor_feedback_coalesced: Arc<AtomicU64>,
) -> Result<()> {
    socket
        .set_nonblocking(true)
        .context("make media socket nonblocking")?;
    let mut offer_active = false;
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
                rtc = fresh_epoch(&socket, host_addr, mapped, bitrate)?;
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
            Ok(TransportCommand::RemoteCandidate(candidate)) => {
                rtc.add_remote_candidate(&candidate);
                mutated = true;
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
                break;
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
                    Ok(true) => {
                        counters.admitted_frames += 1;
                        counters.admitted_bytes = counters.admitted_bytes.saturating_add(au_bytes);
                        debug!(au_age_us = age_us, "RTC access unit written");
                        emit(
                            &events,
                            TransportEvent::AccessUnitWritten {
                                id: au.id,
                                is_idr: au.is_idr,
                            },
                        );
                    }
                    Ok(false) => emit(
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
            match socket.recv_from(&mut buffer) {
                Ok((n, source)) => {
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
        if !mutated && now >= timeout {
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
        // std has no select over UDP + channels.  This bounded receive wakes
        // immediately for producer/control work and never uses a 1 ms sleep;
        // UDP is rechecked at a small ceiling while str0m deadlines win.
        let wait = timeout
            .saturating_duration_since(Instant::now())
            .min(UDP_POLL_CEILING);
        match command_rx.recv_timeout(wait) {
            _ if stop_takes_priority(stop.load(Ordering::Acquire)) => break,
            Ok(TransportCommand::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                running = false
            }
            Ok(command) => {
                match command {
                    TransportCommand::Offer(s) => {
                        rtc = fresh_epoch(&socket, host_addr, mapped, bitrate)?;
                        reliable_cursor_pending = None;
                        cursor_latest
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .take();
                        match rtc.accept_offer(&s) {
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
                    }
                    TransportCommand::RemoteCandidate(c) => rtc.add_remote_candidate(&c),
                    TransportCommand::CursorStateReliable(state) => {
                        reliable_cursor_pending = Some(state)
                    }
                    TransportCommand::Shutdown => {}
                };
                timeout = schedule_timeout(
                    drain_outputs(&mut rtc, &socket, Some(&events), Some(&mut counters))?,
                    Instant::now(),
                    offer_active,
                );
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        dead_epoch_action, emit, replace_latest_cursor, schedule_timeout, stop_takes_priority,
        try_admit_access_unit, try_send_command, DeadEpochAction, TransportCounters,
        TransportEvent,
    };
    use std::sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{channel, sync_channel},
        Mutex,
    };

    #[test]
    fn rtc_epoch_without_a_host_address_starts_without_candidates() {
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        assert!(super::fresh_epoch(&socket, None, None, 2_000_000).is_ok());
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
        assert_eq!(schedule_timeout(now - std::time::Duration::from_millis(1), now, true), now);
    }
}
