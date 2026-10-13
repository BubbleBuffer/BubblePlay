//! LocalService-side client for the privileged BubblePlay Agent.
//!
//! Pipe I/O lives on dedicated threads so a wedged capture/encoder cannot block
//! the RTC/signaling loop. One serialized pump owns the synchronous duplex
//! control handle; the media pipe retains its dedicated blocking reader. The
//! public API permits one encode request at a time and rejects stale
//! generation/revision results before they reach the network transport.

use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use std::{ffi::c_void, os::windows::io::AsRawHandle};

use crate::activity::Activity;
use anyhow::{bail, Context, Result};

use crate::privileged_ipc::{read_frame, write_frame, Message, PermitIdentity};
use crate::privileged_pipe::{PipeRole, PipeStream, RetryPolicy};

const EVENT_CAPACITY: usize = 64;
const MEDIA_CAPACITY: usize = 2;
const CONTROL_CAPACITY: usize = 8;
const LEASE_RENEW_INTERVAL: Duration = Duration::from_millis(500);
const CONTROL_WRITE_TIMEOUT: Duration = Duration::from_millis(250);
const CONTROL_STARTUP_WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const CONTROL_IDLE_POLL: Duration = Duration::from_millis(1);
const ENCODE_RESULT_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Debug)]
enum PipeEvent {
    Message(Message),
    Closed,
    Failed(String),
}

#[derive(Clone, Default)]
struct EventWake(Arc<Mutex<Option<Activity>>>);

impl EventWake {
    fn attach(&self, activity: Activity) {
        *self.0.lock().unwrap_or_else(|error| error.into_inner()) = Some(activity.clone());
        // Readers may already have published events before attachment. The
        // producer must inspect those queues even without another pipe read.
        activity.notify();
    }

    fn notify(&self) {
        let activity = self
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        if let Some(activity) = activity {
            activity.notify();
        }
    }
}

fn publish_event(events: &SyncSender<PipeEvent>, wake: &EventWake, event: PipeEvent) -> bool {
    let published = events.send(event).is_ok();
    // Notify only after queue publication, including when a full queue made
    // send block. A failed send also exposes a stopped reader to its owner.
    wake.notify();
    published
}

struct ControlWrite {
    sequence: u32,
    message: Message,
    result: SyncSender<std::result::Result<(), String>>,
}

#[derive(Debug, Default)]
struct PermitGate {
    outstanding: Option<PermitIdentity>,
}

impl PermitGate {
    fn begin(&mut self, identity: PermitIdentity) -> Result<()> {
        if self.outstanding.is_some() {
            bail!("an Agent encode permit is already outstanding");
        }
        self.outstanding = Some(identity);
        Ok(())
    }

    fn accept(&mut self, message: &Message) -> bool {
        if message.permit_identity() != self.outstanding {
            return false;
        }
        self.outstanding = None;
        true
    }

    fn cancel(&mut self) {
        self.outstanding = None;
    }
}

pub struct AgentClient {
    control_writes: SyncSender<ControlWrite>,
    control_events: Receiver<PipeEvent>,
    media_events: Receiver<PipeEvent>,
    workers: Vec<JoinHandle<()>>,
    next_sequence: u32,
    lease_id: u64,
    next_lease: Instant,
    permits: PermitGate,
    event_wake: EventWake,
}

impl AgentClient {
    pub fn connect() -> Result<Self> {
        Self::connect_with_retry(RetryPolicy::default())
    }

    pub fn connect_with_retry(policy: RetryPolicy) -> Result<Self> {
        let control_writer = PipeStream::connect_with_retry(PipeRole::Control, policy)
            .context("connect Agent control pipe")?;
        let media_reader = PipeStream::connect_with_retry(PipeRole::Media, policy)
            .context("connect Agent media pipe")?;
        Self::from_streams(control_writer, media_reader, CONTROL_STARTUP_WRITE_TIMEOUT)
    }

    fn from_streams(
        control_writer: PipeStream,
        media_reader: PipeStream,
        startup_write_timeout: Duration,
    ) -> Result<Self> {
        let (control_tx, control_events) = mpsc::sync_channel(EVENT_CAPACITY);
        let (media_tx, media_events) = mpsc::sync_channel(MEDIA_CAPACITY);
        let (control_writes, control_write_rx) = mpsc::sync_channel(CONTROL_CAPACITY);
        let event_wake = EventWake::default();
        let workers = vec![
            spawn_control_pump(
                control_writer,
                control_write_rx,
                control_tx,
                event_wake.clone(),
            )?,
            spawn_reader(
                "bubbleplay-agent-media-reader",
                media_reader,
                media_tx,
                event_wake.clone(),
            )?,
        ];
        let lease_id = lease_id();
        let mut client = Self {
            control_writes,
            control_events,
            media_events,
            workers,
            next_sequence: 1,
            lease_id,
            next_lease: Instant::now() + LEASE_RENEW_INTERVAL,
            permits: PermitGate::default(),
            event_wake,
        };
        client
            .send_with_timeout(
                &Message::RenewLease {
                    lease_id,
                    ttl_ms: 2_000,
                },
                startup_write_timeout,
            )
            .context("start Agent lease")?;
        Ok(client)
    }

    /// Wake the producer for every control/media message, error or closure.
    /// Reattachment also wakes for events already queued by the pipe workers.
    pub fn set_activity(&self, activity: Activity) {
        self.event_wake.attach(activity);
    }

    pub fn tick(&mut self, now: Instant) -> Result<()> {
        if now >= self.next_lease {
            self.send(&Message::RenewLease {
                lease_id: self.lease_id,
                ttl_ms: 2_000,
            })?;
            self.next_lease = now + LEASE_RENEW_INTERVAL;
        }
        Ok(())
    }

    /// Start a new input producer using the existing lease identity boundary.
    /// Ordinary heartbeats retain this identity and therefore retain sequencing.
    pub fn begin_viewer_session(&mut self) -> Result<()> {
        let next_lease_id = self.lease_id.wrapping_add(1).max(1);
        self.permits.cancel();
        self.send(&Message::RenewLease {
            lease_id: next_lease_id,
            ttl_ms: 2_000,
        })?;
        self.lease_id = next_lease_id;
        self.next_lease = Instant::now() + LEASE_RENEW_INTERVAL;
        Ok(())
    }

    pub fn send(&mut self, message: &Message) -> Result<()> {
        self.send_with_timeout(message, CONTROL_WRITE_TIMEOUT)
    }

    fn send_with_timeout(&mut self, message: &Message, timeout: Duration) -> Result<()> {
        dispatch_control_write_or_cancel(
            &self.control_writes,
            &self.workers,
            self.next_sequence,
            message,
            timeout,
        )
        .context("write Agent control message")?;
        self.next_sequence = self.next_sequence.wrapping_add(1).max(1);
        Ok(())
    }

    /// Sends one permit and waits only for its correlated media result. Stale
    /// results from a prior process/session generation are discarded.
    pub fn encode(&mut self, permit: Message) -> Result<Message> {
        let identity = permit
            .permit_identity()
            .context("Agent encode request has no permit identity")?;
        self.permits.begin(identity)?;
        if let Err(error) = self.send(&permit) {
            self.permits.cancel();
            return Err(error);
        }
        let deadline = Instant::now() + ENCODE_RESULT_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                self.permits.cancel();
                bail!("Agent encode permit timed out");
            }
            match self.media_events.recv_timeout(remaining) {
                Ok(PipeEvent::Message(message)) if self.permits.accept(&message) => {
                    return Ok(message)
                }
                Ok(PipeEvent::Message(_)) => continue,
                Ok(PipeEvent::Closed) => {
                    self.permits.cancel();
                    bail!("Agent media pipe closed")
                }
                Ok(PipeEvent::Failed(error)) => {
                    self.permits.cancel();
                    bail!("Agent media pipe failed: {error}")
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    self.permits.cancel();
                    bail!("Agent encode permit timed out")
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.permits.cancel();
                    bail!("Agent media reader stopped")
                }
            }
        }
    }

    pub fn try_event(&self) -> Result<Option<Message>> {
        match self.control_events.try_recv() {
            Ok(PipeEvent::Message(message)) => Ok(Some(message)),
            Ok(PipeEvent::Closed) => bail!("Agent control pipe closed"),
            Ok(PipeEvent::Failed(error)) => bail!("Agent control pipe failed: {error}"),
            Err(mpsc::TryRecvError::Empty) => Ok(None),
            Err(mpsc::TryRecvError::Disconnected) => bail!("Agent control reader stopped"),
        }
    }
}

impl Drop for AgentClient {
    fn drop(&mut self) {
        let _ = self.send(&Message::ReleaseAll);
        // Cancel any synchronous pipe reads/writes before detaching the
        // handles so a timed-out session closes and the Agent can accept a
        // replacement connection. Drop must not join potentially broken I/O.
        cancel_worker_io(&self.workers);
        self.workers.clear();
    }
}

fn dispatch_control_write(
    writes: &SyncSender<ControlWrite>,
    sequence: u32,
    message: &Message,
    timeout: Duration,
) -> Result<()> {
    let (result_tx, result_rx) = mpsc::sync_channel(1);
    match writes.try_send(ControlWrite {
        sequence,
        message: message.clone(),
        result: result_tx,
    }) {
        Ok(()) => {}
        Err(mpsc::TrySendError::Full(_)) => bail!("Agent control writer queue is full"),
        Err(mpsc::TrySendError::Disconnected(_)) => bail!("Agent control writer stopped"),
    }
    match result_rx.recv_timeout(timeout) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => bail!("Agent control pipe write failed: {error}"),
        Err(mpsc::RecvTimeoutError::Timeout) => bail!("Agent control pipe write timed out"),
        Err(mpsc::RecvTimeoutError::Disconnected) => bail!("Agent control writer stopped"),
    }
}

fn dispatch_control_write_or_cancel(
    writes: &SyncSender<ControlWrite>,
    workers: &[JoinHandle<()>],
    sequence: u32,
    message: &Message,
    timeout: Duration,
) -> Result<()> {
    let result = dispatch_control_write(writes, sequence, message, timeout);
    if result.is_err() {
        cancel_worker_io(workers);
    }
    result
}

fn spawn_control_pump(
    mut pipe: PipeStream,
    writes: Receiver<ControlWrite>,
    events: SyncSender<PipeEvent>,
    wake: EventWake,
) -> Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("bubbleplay-agent-control-pump".into())
        .spawn(move || loop {
            let mut progressed = false;
            loop {
                let write = match writes.try_recv() {
                    Ok(write) => write,
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => return,
                };
                progressed = true;
                let result = write_frame(&mut pipe, 0, write.sequence, &write.message)
                    .map_err(|error| error.to_string());
                let failed = result.as_ref().err().cloned();
                let _ = write.result.send(result);
                if let Some(error) = failed {
                    publish_event(&events, &wake, PipeEvent::Failed(error));
                    return;
                }
            }

            match pipe.available_bytes() {
                Ok(0) => {}
                Ok(_) => {
                    progressed = true;
                    match read_frame(&mut pipe) {
                        Ok(Some(frame)) => {
                            if !publish_event(&events, &wake, PipeEvent::Message(frame.message)) {
                                return;
                            }
                        }
                        Ok(None) => {
                            publish_event(&events, &wake, PipeEvent::Closed);
                            return;
                        }
                        Err(error) => {
                            publish_event(&events, &wake, PipeEvent::Failed(error.to_string()));
                            return;
                        }
                    }
                }
                Err(error) => {
                    publish_event(&events, &wake, PipeEvent::Failed(error.to_string()));
                    return;
                }
            }

            if !progressed {
                thread::sleep(CONTROL_IDLE_POLL);
            }
        })
        .context("spawn Agent control pump")
}

fn spawn_reader(
    name: &'static str,
    mut pipe: PipeStream,
    events: SyncSender<PipeEvent>,
    wake: EventWake,
) -> Result<JoinHandle<()>> {
    thread::Builder::new()
        .name(name.into())
        .spawn(move || loop {
            match read_frame(&mut pipe) {
                Ok(Some(frame)) => {
                    if !publish_event(&events, &wake, PipeEvent::Message(frame.message)) {
                        break;
                    }
                }
                Ok(None) => {
                    publish_event(&events, &wake, PipeEvent::Closed);
                    break;
                }
                Err(error) => {
                    publish_event(&events, &wake, PipeEvent::Failed(error.to_string()));
                    break;
                }
            }
        })
        .context("spawn Agent pipe reader")
}

fn cancel_worker_io(workers: &[JoinHandle<()>]) -> usize {
    workers
        .iter()
        .filter(|worker| unsafe { CancelSynchronousIo(worker.as_raw_handle()) } != 0)
        .count()
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CancelSynchronousIo(thread_handle: *mut c_void) -> i32;
}

fn lease_id() -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::process::id().hash(&mut hasher);
    Instant::now().hash(&mut hasher);
    hasher.finish().max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::privileged_ipc::NoFrameReason;
    use std::ffi::c_void;
    use std::fs::File;
    use std::io::Read;
    use std::os::windows::io::FromRawHandle;
    use std::ptr;

    fn capabilities() -> Message {
        Message::Capabilities(crate::privileged_ipc::Capabilities {
            flags: 7,
            max_width: 1920,
            max_height: 1080,
            max_fps: 60,
            max_bitrate_bps: 12_000_000,
        })
    }

    #[test]
    fn beginning_viewer_rotates_existing_lease_and_cancels_old_permit() {
        let (writes, requests) = mpsc::sync_channel::<ControlWrite>(1);
        let (_control_tx, control_events) = mpsc::sync_channel(1);
        let (_media_tx, media_events) = mpsc::sync_channel(1);
        let writer = thread::spawn(move || {
            let renewal = requests.recv().unwrap();
            assert!(matches!(
                renewal.message,
                Message::RenewLease { lease_id: 11, .. }
            ));
            renewal.result.send(Ok(())).unwrap();
            let heartbeat = requests.recv().unwrap();
            assert!(matches!(
                heartbeat.message,
                Message::RenewLease { lease_id: 11, .. }
            ));
            heartbeat.result.send(Ok(())).unwrap();
            let release = requests.recv().unwrap();
            assert!(matches!(release.message, Message::ReleaseAll));
            release.result.send(Ok(())).unwrap();
        });
        let mut client = AgentClient {
            control_writes: writes,
            control_events,
            media_events,
            workers: Vec::new(),
            next_sequence: 1,
            lease_id: 10,
            next_lease: Instant::now(),
            permits: PermitGate::default(),
            event_wake: EventWake::default(),
        };
        client
            .permits
            .begin(PermitIdentity {
                permit_id: 1,
                session_generation: 1,
                config_revision: 1,
            })
            .unwrap();
        client.begin_viewer_session().unwrap();
        assert!(client.permits.outstanding.is_none());
        client.tick(Instant::now() + LEASE_RENEW_INTERVAL).unwrap();
        assert_eq!(client.lease_id, 11);
        drop(client);
        writer.join().unwrap();
    }

    #[test]
    fn agent_control_message_interrupts_a_long_producer_activity_wait() {
        let (client_control, mut agent_control) =
            crate::privileged_pipe::connected_test_pair(PipeRole::Control).unwrap();
        let (client_media, _agent_media) =
            crate::privileged_pipe::connected_test_pair(PipeRole::Media).unwrap();
        let (trigger_tx, trigger_rx) = mpsc::sync_channel(1);
        let agent = thread::spawn(move || {
            assert!(matches!(
                read_frame(&mut agent_control).unwrap().unwrap().message,
                Message::RenewLease { .. }
            ));
            trigger_rx.recv().unwrap();
            write_frame(&mut agent_control, 0, 1, &capabilities()).unwrap();
            // Retain the pipe until client cleanup so a closure cannot be
            // mistaken for the control message's wake in this test.
            assert!(matches!(
                read_frame(&mut agent_control).unwrap().unwrap().message,
                Message::ReleaseAll
            ));
        });
        let client =
            AgentClient::from_streams(client_control, client_media, CONTROL_STARTUP_WRITE_TIMEOUT)
                .unwrap();
        let activity = Activity::new().unwrap();
        client.set_activity(activity.clone());
        activity.wait(Duration::ZERO, &[]).unwrap(); // Drain the attach pulse.
        let trigger = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            trigger_tx.send(()).unwrap();
        });
        let started = Instant::now();
        activity.wait(Duration::from_secs(5), &[]).unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(matches!(
            client.try_event().unwrap(),
            Some(Message::Capabilities(_))
        ));
        trigger.join().unwrap();
        drop(client);
        agent.join().unwrap();
    }

    #[test]
    fn attach_and_reattach_wake_for_already_published_agent_events() {
        let wake = EventWake::default();
        let (events, received) = mpsc::sync_channel(2);
        assert!(publish_event(
            &events,
            &wake,
            PipeEvent::Message(capabilities())
        ));
        assert!(publish_event(&events, &wake, PipeEvent::Closed));
        for expected_message in [true, false] {
            let activity = Activity::new().unwrap();
            wake.attach(activity.clone());
            let started = Instant::now();
            activity.wait(Duration::from_secs(5), &[]).unwrap();
            assert!(started.elapsed() < Duration::from_secs(1));
            let event = received.try_recv().unwrap();
            assert_eq!(matches!(event, PipeEvent::Message(_)), expected_message);
        }
    }

    #[test]
    fn bounded_agent_queue_notifies_after_backpressure_releases_publication() {
        let wake = EventWake::default();
        let activity = Activity::new().unwrap();
        wake.attach(activity.clone());
        activity.wait(Duration::ZERO, &[]).unwrap();
        let (events, received) = mpsc::sync_channel(1);
        assert!(publish_event(
            &events,
            &wake,
            PipeEvent::Message(capabilities())
        ));
        activity.wait(Duration::ZERO, &[]).unwrap();
        let worker_wake = wake.clone();
        let worker = thread::spawn(move || {
            assert!(publish_event(
                &events,
                &worker_wake,
                PipeEvent::Failed("injected pipe failure".into())
            ));
        });
        // The second publication cannot complete until this dequeue.
        assert!(matches!(received.recv().unwrap(), PipeEvent::Message(_)));
        let started = Instant::now();
        activity.wait(Duration::from_secs(5), &[]).unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(matches!(received.try_recv().unwrap(), PipeEvent::Failed(_)));
        worker.join().unwrap();
    }

    #[test]
    fn startup_lease_write_completes_while_agent_has_no_inbound_event() {
        let (client_control, mut agent_control) =
            crate::privileged_pipe::connected_test_pair(PipeRole::Control).unwrap();
        let (client_media, _agent_media) =
            crate::privileged_pipe::connected_test_pair(PipeRole::Media).unwrap();
        let (lease_tx, lease_rx) = mpsc::sync_channel(1);
        let agent = thread::spawn(move || {
            let message = read_frame(&mut agent_control)
                .unwrap()
                .expect("startup lease frame")
                .message;
            lease_tx.send(message).unwrap();
        });

        let client =
            AgentClient::from_streams(client_control, client_media, Duration::from_millis(100))
                .expect("startup lease must not be blocked by an idle control read");
        assert!(matches!(
            lease_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            Message::RenewLease { .. }
        ));

        drop(client);
        agent.join().unwrap();
    }

    #[test]
    fn control_pump_remains_bidirectional_across_periodic_lease_renewal() {
        let (client_control, mut agent_control) =
            crate::privileged_pipe::connected_test_pair(PipeRole::Control).unwrap();
        let (client_media, _agent_media) =
            crate::privileged_pipe::connected_test_pair(PipeRole::Media).unwrap();
        let (renewed_tx, renewed_rx) = mpsc::sync_channel(1);
        let agent = thread::spawn(move || {
            assert!(matches!(
                read_frame(&mut agent_control).unwrap().unwrap().message,
                Message::RenewLease { .. }
            ));
            write_frame(
                &mut agent_control,
                0,
                1,
                &Message::Capabilities(crate::privileged_ipc::Capabilities {
                    flags: 7,
                    max_width: 1920,
                    max_height: 1080,
                    max_fps: 60,
                    max_bitrate_bps: 12_000_000,
                }),
            )
            .unwrap();
            assert!(matches!(
                read_frame(&mut agent_control).unwrap().unwrap().message,
                Message::RenewLease { .. }
            ));
            renewed_tx.send(()).unwrap();
        });

        let mut client =
            AgentClient::from_streams(client_control, client_media, Duration::from_millis(100))
                .unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        let event = loop {
            if let Some(event) = client.try_event().unwrap() {
                break event;
            }
            assert!(Instant::now() < deadline, "Agent event was not received");
            thread::sleep(Duration::from_millis(1));
        };
        assert!(matches!(event, Message::Capabilities(_)));

        client.tick(Instant::now() + LEASE_RENEW_INTERVAL).unwrap();
        renewed_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        drop(client);
        agent.join().unwrap();
    }

    #[test]
    fn dropping_client_sends_release_and_stops_idle_control_pump() {
        let (client_control, mut agent_control) =
            crate::privileged_pipe::connected_test_pair(PipeRole::Control).unwrap();
        let (client_media, _agent_media) =
            crate::privileged_pipe::connected_test_pair(PipeRole::Media).unwrap();
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let agent = thread::spawn(move || {
            assert!(matches!(
                read_frame(&mut agent_control).unwrap().unwrap().message,
                Message::RenewLease { .. }
            ));
            let released = matches!(
                read_frame(&mut agent_control).unwrap().unwrap().message,
                Message::ReleaseAll
            );
            release_tx.send(released).unwrap();
        });

        let client =
            AgentClient::from_streams(client_control, client_media, Duration::from_millis(100))
                .unwrap();
        drop(client);

        assert!(release_rx.recv_timeout(Duration::from_secs(1)).unwrap());
        agent.join().unwrap();
    }

    #[test]
    fn idle_control_poll_is_bounded_to_two_milliseconds() {
        assert!(CONTROL_IDLE_POLL >= Duration::from_millis(1));
        assert!(CONTROL_IDLE_POLL <= Duration::from_millis(2));
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreatePipe(
            read_pipe: *mut *mut c_void,
            write_pipe: *mut *mut c_void,
            pipe_attributes: *const c_void,
            size: u32,
        ) -> i32;
    }

    #[test]
    fn stalled_agent_control_write_is_bounded() {
        let (writes, _stalled_writer) = mpsc::sync_channel(1);
        let started = Instant::now();
        let error = dispatch_control_write(
            &writes,
            7,
            &Message::ListMonitors,
            Duration::from_millis(20),
        )
        .unwrap_err();

        assert!(error.to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_millis(250));
    }

    #[test]
    fn cold_start_control_write_has_a_distinct_bounded_grace_window() {
        assert!(CONTROL_STARTUP_WRITE_TIMEOUT >= Duration::from_secs(1));
        assert!(CONTROL_STARTUP_WRITE_TIMEOUT <= Duration::from_secs(2));
        assert!(CONTROL_STARTUP_WRITE_TIMEOUT > CONTROL_WRITE_TIMEOUT);
    }

    #[test]
    fn cancellation_interrupts_blocked_worker_pipe_io() {
        let mut read_pipe = ptr::null_mut();
        let mut write_pipe = ptr::null_mut();
        assert_ne!(
            unsafe { CreatePipe(&mut read_pipe, &mut write_pipe, ptr::null(), 0) },
            0
        );
        let mut reader = unsafe { File::from_raw_handle(read_pipe) };
        let writer = unsafe { File::from_raw_handle(write_pipe) };
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            let _ = started_tx.send(());
            let mut byte = [0u8; 1];
            let _ = done_tx.send(reader.read(&mut byte));
        });
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        thread::sleep(Duration::from_millis(20));

        let cancelled = cancel_worker_io(std::slice::from_ref(&worker));
        let result = done_rx.recv_timeout(Duration::from_secs(1));
        drop(writer);
        worker.join().unwrap();

        assert_eq!(cancelled, 1);
        let error = result.unwrap().unwrap_err();
        assert_eq!(error.raw_os_error(), Some(995));
    }

    #[test]
    fn failed_control_write_cancels_blocked_worker_io() {
        let (writes, _stalled_writer) = mpsc::sync_channel(1);
        let mut read_pipe = ptr::null_mut();
        let mut write_pipe = ptr::null_mut();
        assert_ne!(
            unsafe { CreatePipe(&mut read_pipe, &mut write_pipe, ptr::null(), 0) },
            0
        );
        let mut reader = unsafe { File::from_raw_handle(read_pipe) };
        let writer = unsafe { File::from_raw_handle(write_pipe) };
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            let _ = started_tx.send(());
            let mut byte = [0u8; 1];
            let _ = done_tx.send(reader.read(&mut byte));
        });
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        thread::sleep(Duration::from_millis(20));

        let error = dispatch_control_write_or_cancel(
            &writes,
            std::slice::from_ref(&worker),
            8,
            &Message::ListMonitors,
            Duration::from_millis(20),
        )
        .unwrap_err();
        let result = done_rx.recv_timeout(Duration::from_secs(1));
        drop(writer);
        worker.join().unwrap();

        assert!(error.to_string().contains("timed out"));
        let error = result.unwrap().unwrap_err();
        assert_eq!(error.raw_os_error(), Some(995));
    }

    fn identity(id: u64, session: u32, revision: u32) -> PermitIdentity {
        PermitIdentity {
            permit_id: id,
            session_generation: session,
            config_revision: revision,
        }
    }

    fn no_frame(value: PermitIdentity) -> Message {
        Message::NoFrame {
            permit_id: value.permit_id,
            session_generation: value.session_generation,
            config_revision: value.config_revision,
            reason: NoFrameReason::NoChange,
        }
    }

    #[test]
    fn permit_gate_accepts_only_the_exact_generation_revision_and_id() {
        let current = identity(9, 4, 7);
        let mut gate = PermitGate::default();
        gate.begin(current).unwrap();
        assert!(!gate.accept(&no_frame(identity(8, 4, 7))));
        assert!(!gate.accept(&no_frame(identity(9, 3, 7))));
        assert!(!gate.accept(&no_frame(identity(9, 4, 6))));
        assert!(gate.accept(&no_frame(current)));
        assert!(gate.outstanding.is_none());
    }

    #[test]
    fn only_one_permit_can_be_outstanding() {
        let mut gate = PermitGate::default();
        gate.begin(identity(1, 1, 1)).unwrap();
        assert!(gate.begin(identity(2, 1, 1)).is_err());
        gate.cancel();
        assert!(gate.begin(identity(2, 1, 1)).is_ok());
    }
}
