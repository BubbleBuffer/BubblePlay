//! Epoch-scoped input mailbox. Only the /ws worker touches the socket.
//! UI producers never wait for network I/O; a full reliable queue releases
//! the remote state before accepting more input. Motion is coalesced by the
//! capture before admission, and the queue has the browser's 4 MiB limit.
use crate::input::protocol as input_wire;
use std::{
    collections::{BTreeSet, VecDeque},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use tokio::sync::Notify;

const MAX_BYTES: usize = 4 * 1024 * 1024;
const RELEASE_ALL: u8 = 6;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct CursorState {
    pub ack: u16,
    pub x: u16,
    pub y: u16,
    pub has_ack: bool,
    pub in_bounds: bool,
    pub visible: bool,
}

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct InputSnapshot {
    pub epoch: u64,
    pub ready: bool,
    /// Readiness transitions from open to closed, retained across epochs.
    pub gate_closes: u64,
    /// Monitor carries that reopened the gate, retained across epochs.
    pub monitor_resumes: u64,
    pub scancode: bool,
    pub width: u32,
    pub height: u32,
    pub cursor: Option<CursorState>,
    pub cursor_kind: u8,
    pub sent: u64,
    pub refused: u64,
    pub releases: u64,
    pub cursor_states: u64,
    pub cursor_kinds: u64,
    pub rtt_samples: u64,
    pub rtt_p50_ms: Option<f64>,
    pub rtt_p95_ms: Option<f64>,
}

#[derive(Default)]
struct Inner {
    view: InputSnapshot,
    queue: VecDeque<Vec<u8>>,
    bytes: usize,
    needs_release: bool,
    absolute_sent: VecDeque<(u16, Instant)>,
    rtt: VecDeque<f64>,
    /// A replacement capture restarts its private allocator at zero. The
    /// host retains sequence watermarks across release-all and mode changes.
    sequence_offset: u16,
    next_sequence: u16,
    /// Keys the host holds, from admitted presses. Cleared by every release barrier.
    held: BTreeSet<HeldKey>,
    /// Buttons the host holds, from admitted button packets.
    button_mask: u8,
    /// A monitor transition carries held input while the transport gate is closed.
    hold: bool,
    /// A gated release of held input happened during the hold: resume sends one release-all.
    owed: bool,
    /// The current capture uses Work cursor sync. Re-arm it after monitor resume
    /// in this shared sequence space, without changing the capture's epoch.
    resync_cursor: bool,
}

#[derive(Clone, Default)]
pub struct InputLink {
    inner: Arc<Mutex<Inner>>,
    pub(crate) notify: Arc<Notify>,
    wake: Option<Arc<dyn Fn() + Send + Sync>>,
    ready_signal: Arc<AtomicBool>,
    /// What a capture view reports: open, or carried through a monitor hold.
    capture_signal: Arc<AtomicBool>,
    /// True only on a view from `capture_view`.
    capture: bool,
}

/// A key as the host identifies it: a VK, or a scan code with its extended bit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum HeldKey {
    Vk(u16),
    Scan { scan: u16, extended: bool },
}
impl std::fmt::Debug for InputLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InputLink")
            .field("snapshot", &self.snapshot())
            .finish()
    }
}
impl InputLink {
    /// Restore an unsent reliable packet without admitting newer events ahead
    /// of it. The network thread calls this when SCTP applies backpressure.
    pub(crate) fn retry_front(&self, packet: Vec<u8>) {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if packet == [6] {
            state.needs_release = true;
            return;
        }
        if state.view.ready && state.bytes + packet.len() <= MAX_BYTES {
            state.bytes += packet.len();
            state.queue.push_front(packet);
        } else {
            barrier(&mut state);
        }
    }
    pub fn ready_signal(&self) -> Arc<AtomicBool> {
        if self.capture {
            self.capture_signal.clone()
        } else {
            self.ready_signal.clone()
        }
    }
    /// A producer view for the capture. During a monitor hold it reports ready
    /// and consumes gated input instead of refusing it. Only the view's
    /// `snapshot().ready` and `ready_signal()` change; the transport handle's
    /// stay closed until the new frame opens the gate.
    pub fn capture_view(&self) -> InputLink {
        Self {
            capture: true,
            ..self.clone()
        }
    }
    pub fn new(wake: Arc<dyn Fn() + Send + Sync>) -> Self {
        Self {
            wake: Some(wake),
            ..Self::default()
        }
    }
    pub fn snapshot(&self) -> InputSnapshot {
        let state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut view = state.view.clone();
        if self.capture {
            view.ready = view.ready || state.hold;
        }
        view
    }
    /// Both signals follow the mailbox state; the transport signal never reports a hold.
    fn publish_ready(&self, state: &Inner) {
        self.ready_signal.store(state.view.ready, Ordering::Release);
        self.capture_signal
            .store(state.view.ready || state.hold, Ordering::Release);
    }
    /// A monitor transition closes the transport gate but carries held input
    /// (browser `inputTransitionHold`). Only input that was already admitted is
    /// carried; before the first admission there is nothing to carry.
    pub(crate) fn hold_for_monitor(&self) {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if state.hold || !state.view.ready {
            return;
        }
        state.hold = true;
        state.view.ready = false;
        state.view.gate_closes += 1;
        // Motion queued before the gate is stale on the new monitor (browser clearMotion).
        state.queue.retain(|packet| {
            !matches!(
                packet.first().copied(),
                Some(input_wire::TYPE_MOTION | input_wire::TYPE_ABSOLUTE)
            )
        });
        state.bytes = state.queue.iter().map(Vec::len).sum();
        self.publish_ready(&state);
        drop(state);
        self.notify.notify_one();
        self.wake_ui();
    }
    /// True while a monitor transition carries held input.
    pub(crate) fn monitor_hold(&self) -> bool {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).hold
    }
    fn wake_ui(&self) {
        if let Some(wake) = &self.wake {
            wake();
        }
    }
    pub(crate) fn begin_epoch(&self, epoch: u64) {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let (sent, refused, releases) = (state.view.sent, state.view.refused, state.view.releases);
        let gate_closes = state.view.gate_closes + u64::from(state.view.ready);
        let monitor_resumes = state.view.monitor_resumes;
        *state = Inner {
            view: InputSnapshot {
                epoch,
                sent,
                refused,
                releases,
                gate_closes,
                monitor_resumes,
                ..Default::default()
            },
            needs_release: true,
            ..Default::default()
        };
        self.publish_ready(&state);
        drop(state);
        self.wake_ui();
    }
    pub(crate) fn set_ready(&self, ready: bool) {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if state.hold {
            if ready {
                resume(&mut state);
            } else {
                // A closure during a carry (source change, dead transport) releases
                // now, as before. The gate was already counted closed at the hold.
                state.hold = false;
                barrier(&mut state);
            }
            self.publish_ready(&state);
            drop(state);
            self.notify.notify_one();
            self.wake_ui();
            return;
        }
        if state.view.ready == ready {
            return;
        }
        state.view.ready = ready;
        if !ready {
            state.view.gate_closes += 1;
            barrier(&mut state);
        }
        self.publish_ready(&state);
        drop(state);
        self.notify.notify_one();
        self.wake_ui();
    }
    pub(crate) fn dimensions(&self, width: u32, height: u32) {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        state.view.width = width;
        state.view.height = height;
    }
    /// A packet is accepted only for the current ready epoch. The caller
    /// returns a sequence number to its allocator if this refuses admission.
    /// A capture view during a monitor hold consumes gated input instead.
    pub fn send(&self, epoch: u64, mut packet: Vec<u8>) -> bool {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if self.capture && state.hold && epoch == state.view.epoch {
            return gate(&mut state, &packet);
        }
        if !state.view.ready || epoch != state.view.epoch {
            state.view.refused += 1;
            return false;
        }
        if state.bytes + packet.len() > MAX_BYTES {
            barrier(&mut state);
            state.view.refused += 1;
            drop(state);
            self.notify.notify_one();
            return false;
        }
        match (packet.first().copied(), packet.len()) {
            (Some(input_wire::TYPE_MOTION), 11)
            | (Some(input_wire::TYPE_ABSOLUTE), 7)
            | (Some(input_wire::TYPE_BUTTONS_AT), 8)
            | (Some(input_wire::TYPE_CURSOR_SYNC), 3) => {
                // A capture view takes the mailbox's next number, so input a gate
                // consumed leaves no hole in the wire sequence.
                let wire = if self.capture {
                    state.next_sequence
                } else {
                    let local = u16::from_le_bytes([packet[1], packet[2]]);
                    local.wrapping_add(state.sequence_offset)
                };
                packet[1..3].copy_from_slice(&wire.to_le_bytes());
                state.next_sequence = wire.wrapping_add(1);
            }
            _ => {}
        }
        track(&mut state, &packet);
        if self.capture && packet.first() == Some(&input_wire::TYPE_CURSOR_SYNC) {
            state.resync_cursor = true;
        }
        state.bytes += packet.len();
        state.queue.push_back(packet);
        drop(state);
        self.notify.notify_one();
        true
    }
    /// Replace a capture whose local allocator starts at zero, while keeping
    /// the browser's shared sequence space on the wire until the epoch ends.
    pub(crate) fn restart_producer(&self) {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        state.sequence_offset = state.next_sequence;
        state.resync_cursor = false;
        // The replaced capture's carry ends with it, so its held input is released.
        state.hold = false;
        barrier(&mut state);
        self.publish_ready(&state);
        drop(state);
        self.notify.notify_one();
    }
    /// Release is a barrier: stale queued input is discarded. When the lane
    /// is gated/dead, retain the release for the first ready event on resume.
    pub fn release(&self, epoch: u64) {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if state.view.epoch != epoch {
            return;
        }
        barrier(&mut state);
        drop(state);
        self.notify.notify_one();
    }
    /// Input the transport could write now: queued packets or an owed release barrier,
    /// while the gate is open (a closed gate keeps them without sending anything).
    pub(crate) fn has_pending(&self) -> bool {
        let state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        state.view.ready && (state.needs_release || !state.queue.is_empty())
    }
    pub(crate) fn next(&self) -> Option<Vec<u8>> {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if !state.view.ready {
            return None;
        }
        if std::mem::take(&mut state.needs_release) {
            return Some(vec![6]);
        }
        let packet = state.queue.pop_front()?;
        state.bytes -= packet.len();
        Some(packet)
    }
    pub(crate) fn sent(&self, release: bool) {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        state.view.sent += 1;
        if release {
            state.view.releases += 1;
        }
    }
    pub(crate) fn absolute_sent(&self, packet: &[u8], at: Instant) {
        let [8, lo, hi, _, _, _, _] = packet else {
            return;
        };
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let sequence = u16::from_le_bytes([*lo, *hi]);
        state.absolute_sent.retain(|(seq, _)| *seq != sequence);
        while state
            .absolute_sent
            .front()
            .is_some_and(|(_, old)| at.saturating_duration_since(*old) > Duration::from_secs(5))
        {
            state.absolute_sent.pop_front();
        }
        if state.absolute_sent.len() == 512 {
            state.absolute_sent.pop_front();
        }
        state
            .absolute_sent
            .push_back((u16::from_le_bytes([*lo, *hi]), at));
    }
    pub(crate) fn absolute_unsent(&self, packet: &[u8]) {
        if let [8, lo, hi, _, _, _, _] = packet {
            let seq = u16::from_le_bytes([*lo, *hi]);
            self.inner
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .absolute_sent
                .retain(|(s, _)| *s != seq);
        }
    }
    pub(crate) fn receive(&self, data: &[u8]) -> bool {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        match data {
            [128, al, ah, xl, xh, yl, yh, flags] => {
                state.view.cursor_states += 1;
                state.view.cursor = Some(CursorState {
                    ack: u16::from_le_bytes([*al, *ah]),
                    x: u16::from_le_bytes([*xl, *xh]),
                    y: u16::from_le_bytes([*yl, *yh]),
                    has_ack: flags & 1 != 0,
                    in_bounds: flags & 2 != 0,
                    visible: flags & 4 != 0,
                });
                if flags & 1 != 0 {
                    let ack = u16::from_le_bytes([*al, *ah]);
                    let now = Instant::now();
                    if let Some(index) = state.absolute_sent.iter().position(|(seq, at)| {
                        *seq == ack && now.saturating_duration_since(*at) <= Duration::from_secs(5)
                    }) {
                        let (_, at) = state.absolute_sent.remove(index).unwrap();
                        if state.rtt.len() == 512 {
                            state.rtt.pop_front();
                        }
                        state
                            .rtt
                            .push_back(now.saturating_duration_since(at).as_secs_f64() * 1000.0);
                        state.view.rtt_samples += 1;
                        let mut samples: Vec<_> = state.rtt.iter().copied().collect();
                        samples.sort_by(f64::total_cmp);
                        let percentile = |p: f64| {
                            samples[((samples.len() as f64 * p).ceil() as usize).saturating_sub(1)]
                        };
                        state.view.rtt_p50_ms = Some(percentile(0.5));
                        state.view.rtt_p95_ms = Some(percentile(0.95));
                    }
                }
            }
            [129, kind] => {
                state.view.cursor_kinds += 1;
                state.view.cursor_kind = if *kind <= 14 { *kind } else { 0 };
            }
            _ => return false,
        }
        drop(state);
        self.wake_ui();
        true
    }
    pub(crate) fn capabilities(&self, scancode: bool) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .view
            .scancode = scancode;
    }
}

/// A release barrier: queued input is discarded and the host gets release-all
/// before anything newer. Held state is forgotten because the host forgets it.
fn barrier(state: &mut Inner) {
    state.queue.clear();
    state.bytes = 0;
    state.needs_release = true;
    state.owed = false;
    state.held.clear();
    state.button_mask = 0;
}

/// The gate reopens after a monitor transition (browser `resumeAfterTransition`).
/// Input admitted before the gate keeps its place; a gated release goes out
/// after it. Held input is kept when nothing held was let go.
fn resume(state: &mut Inner) {
    state.hold = false;
    state.view.ready = true;
    state.view.monitor_resumes += 1;
    if std::mem::take(&mut state.owed) {
        state.queue.push_back(vec![RELEASE_ALL]);
        state.bytes += 1;
        state.held.clear();
        state.button_mask = 0;
    }
    if state.resync_cursor {
        let sync = input_wire::cursor_sync(state.next_sequence).to_vec();
        state.next_sequence = state.next_sequence.wrapping_add(1);
        state.bytes += sync.len();
        state.queue.push_back(sync);
    }
}

/// Held input was let go during the gate. Its release is owed on resume.
fn owe(state: &mut Inner) {
    state.owed = true;
    state.held.clear();
    state.button_mask = 0;
}

/// The key a packet presses or releases, and whether it releases it.
fn key_event(packet: &[u8]) -> Option<(HeldKey, bool)> {
    match packet {
        [input_wire::TYPE_KEY_DOWN, lo, hi] => {
            Some((HeldKey::Vk(u16::from_le_bytes([*lo, *hi])), false))
        }
        [input_wire::TYPE_KEY_UP, lo, hi] => {
            Some((HeldKey::Vk(u16::from_le_bytes([*lo, *hi])), true))
        }
        [input_wire::TYPE_SCAN_KEY, flags, s0, s1, v0, v1] => {
            let scan = u16::from_le_bytes([*s0, *s1]);
            let key = if scan == 0 {
                HeldKey::Vk(u16::from_le_bytes([*v0, *v1]))
            } else {
                HeldKey::Scan {
                    scan,
                    extended: flags & input_wire::SCAN_FLAG_EXTENDED != 0,
                }
            };
            Some((key, flags & input_wire::SCAN_FLAG_UP != 0))
        }
        _ => None,
    }
}

/// The full button mask a button packet sets.
fn button_mask_of(packet: &[u8]) -> Option<u8> {
    match packet {
        [input_wire::TYPE_BUTTONS, mask] => Some(*mask),
        [input_wire::TYPE_BUTTONS_AT, _, _, _, _, _, _, mask] => Some(*mask),
        _ => None,
    }
}

/// Records what the host holds once an admitted packet is sent.
fn track(state: &mut Inner, packet: &[u8]) {
    if let Some((key, up)) = key_event(packet) {
        if up {
            state.held.remove(&key);
        } else {
            state.held.insert(key);
        }
    } else if let Some(mask) = button_mask_of(packet) {
        state.button_mask = mask;
    } else if matches!(packet, [RELEASE_ALL]) {
        state.held.clear();
        state.button_mask = 0;
    }
}

/// Browser gating (`InputController` while "gated"), for a packet a capture view
/// offers during a monitor hold. Nothing gated is admitted or replayed. Returns
/// the producer's result: true means consumed, and false asks for a retry. Only
/// cursor sync asks for one, so it is sent after the gate reopens.
fn gate(state: &mut Inner, packet: &[u8]) -> bool {
    if let Some((key, up)) = key_event(packet) {
        // A press is dropped without being recorded; a repeat keeps the key held.
        if up && state.held.remove(&key) {
            owe(state);
        }
        return true;
    }
    if let Some(mask) = button_mask_of(packet) {
        // The browser leaves its button mask unchanged while gated.
        if state.button_mask & !mask != 0 {
            owe(state);
        }
        return true;
    }
    match packet {
        [RELEASE_ALL] => {
            if !state.held.is_empty() || state.button_mask != 0 {
                owe(state);
            }
            true
        }
        [input_wire::TYPE_CURSOR_SYNC, ..] => false,
        // Motion, absolute and the remaining kinds are dropped without a release.
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_replacement_keeps_wire_sequence_continuous_until_epoch_restart() {
        let link = InputLink::default();
        link.begin_epoch(1);
        link.set_ready(true);
        link.next();
        assert!(link.send(1, input_wire::absolute(u16::MAX - 1, 100, 200).to_vec()));
        assert_eq!(
            link.next(),
            Some(input_wire::absolute(u16::MAX - 1, 100, 200).to_vec())
        );

        link.restart_producer();
        assert!(link.send(1, input_wire::cursor_sync(0).to_vec()));
        assert!(link.send(1, input_wire::motion(1, 1, 2, 0, 0).to_vec()));
        assert_eq!(link.next(), Some(vec![6]));
        assert_eq!(
            link.next(),
            Some(input_wire::cursor_sync(u16::MAX).to_vec())
        );
        assert_eq!(
            link.next(),
            Some(input_wire::motion(0, 1, 2, 0, 0).to_vec())
        );

        link.restart_producer();
        assert!(link.send(1, input_wire::buttons_at(0, 20, 30, 1).to_vec()));
        assert_eq!(link.next(), Some(vec![6]));
        assert_eq!(
            link.next(),
            Some(input_wire::buttons_at(1, 20, 30, 1).to_vec())
        );
        link.begin_epoch(2);
        link.set_ready(true);
        assert!(link.send(2, input_wire::cursor_sync(0).to_vec()));
        assert_eq!(link.next(), Some(vec![6]));
        assert_eq!(link.next(), Some(input_wire::cursor_sync(0).to_vec()));
    }

    #[test]
    fn closing_readiness_discards_old_input_and_records_real_gate_closures() {
        let link = InputLink::default();
        link.begin_epoch(1);
        link.set_ready(true);
        assert_eq!(link.next(), Some(vec![6]));
        assert!(link.send(1, vec![1, 0x41, 0]));
        link.set_ready(false);
        assert_eq!(link.snapshot().gate_closes, 1);
        assert!(!link.ready_signal().load(Ordering::Acquire));
        assert!(link.next().is_none());
        link.set_ready(false);
        assert_eq!(
            link.snapshot().gate_closes,
            1,
            "already closed is not a new transition"
        );
        link.set_ready(true);
        assert_eq!(
            link.next(),
            Some(vec![6]),
            "release precedes new-desktop input"
        );
        assert!(link.next().is_none(), "the old key was discarded");
        link.begin_epoch(2);
        assert_eq!(link.snapshot().gate_closes, 2);
    }
    #[test]
    fn readiness_epoch_and_release_barriers() {
        let link = InputLink::default();
        link.begin_epoch(1);
        assert!(!link.send(1, vec![4, 1]));
        assert!(link.next().is_none());
        link.set_ready(true);
        assert_eq!(link.next(), Some(vec![6]));
        assert!(link.send(1, vec![4, 1]));
        link.release(1);
        assert_eq!(link.next(), Some(vec![6]));
        assert!(link.next().is_none());
        assert!(link.send(1, vec![4, 1]));
        link.begin_epoch(2);
        link.set_ready(true);
        assert!(!link.send(1, vec![4, 1]));
        assert_eq!(link.next(), Some(vec![6]));
        assert!(link.next().is_none());
    }
    #[test]
    fn cursor_layouts_and_unknown_kinds_match_browser() {
        let link = InputLink::default();
        assert!(link.receive(&[128, 7, 0, 0, 64, 0, 192, 7]));
        assert_eq!(
            link.snapshot().cursor,
            Some(CursorState {
                ack: 7,
                x: 16384,
                y: 49152,
                has_ack: true,
                in_bounds: true,
                visible: true
            })
        );
        assert!(!link.receive(&[128, 0]));
        assert!(!link.receive(&[129, 1, 2]));
        assert!(link.receive(&[129, 14]));
        assert_eq!(link.snapshot().cursor_kind, 14);
        link.receive(&[129, 255]);
        assert_eq!(link.snapshot().cursor_kind, 0);
    }
    #[test]
    fn congestion_is_bounded_and_resynchronizes() {
        let link = InputLink::default();
        link.begin_epoch(1);
        link.set_ready(true);
        link.next();
        assert!(link.send(1, vec![0; MAX_BYTES]));
        assert!(!link.send(1, vec![4, 1]));
        assert_eq!(link.next(), Some(vec![6]));
        assert!(link.next().is_none());
    }
    #[test]
    fn round_trip_requires_exact_current_epoch_absolute_ack_and_counts_once() {
        let link = InputLink::default();
        link.begin_epoch(1);
        link.absolute_sent(
            &[8, 7, 0, 0, 0, 0, 0],
            Instant::now() - Duration::from_millis(12),
        );
        link.receive(&[128, 6, 0, 0, 0, 0, 0, 7]);
        assert_eq!(link.snapshot().rtt_samples, 0);
        link.receive(&[128, 7, 0, 0, 0, 0, 0, 6]);
        assert_eq!(link.snapshot().rtt_samples, 0);
        link.receive(&[128, 7, 0, 0, 0, 0, 0, 7]);
        assert_eq!(link.snapshot().rtt_samples, 1);
        assert!(link.snapshot().rtt_p50_ms.unwrap() >= 12.0);
        link.receive(&[128, 7, 0, 0, 0, 0, 0, 7]);
        assert_eq!(link.snapshot().rtt_samples, 1);
        link.absolute_sent(&[8, 8, 0, 0, 0, 0, 0], Instant::now());
        link.begin_epoch(2);
        link.receive(&[128, 8, 0, 0, 0, 0, 0, 7]);
        assert_eq!(link.snapshot().rtt_samples, 0);
    }

    fn ready_link(epoch: u64) -> InputLink {
        let link = InputLink::default();
        link.begin_epoch(epoch);
        link.set_ready(true);
        assert_eq!(link.next(), Some(vec![6]));
        link
    }

    #[test]
    fn monitor_hold_keeps_held_input_and_closes_only_the_transport_gate() {
        let link = ready_link(1);
        let view = link.capture_view();
        assert!(view.send(1, vec![0, 0x41, 0]));
        assert!(view.send(1, vec![4, 1]));
        assert_eq!(link.next(), Some(vec![0, 0x41, 0]));
        assert_eq!(link.next(), Some(vec![4, 1]));

        link.hold_for_monitor();
        let transport = link.snapshot();
        assert!(!transport.ready);
        assert_eq!(transport.gate_closes, 1);
        assert!(!link.ready_signal().load(Ordering::Acquire));
        assert!(view.snapshot().ready);
        assert!(view.ready_signal().load(Ordering::Acquire));
        assert!(link.next().is_none());

        // A repeat of the held key is dropped and keeps it held.
        assert!(view.send(1, vec![0, 0x41, 0]));
        link.set_ready(true);
        assert!(
            link.next().is_none(),
            "nothing was let go, so no release-all"
        );
        assert_eq!(link.snapshot().gate_closes, 1);
        assert_eq!(link.snapshot().monitor_resumes, 1);
        assert!(link.snapshot().ready);
        // Still held after resume: the later releases are ordinary packets.
        assert!(view.send(1, vec![1, 0x41, 0]));
        assert!(view.send(1, vec![4, 0]));
        assert_eq!(link.next(), Some(vec![1, 0x41, 0]));
        assert_eq!(link.next(), Some(vec![4, 0]));
    }

    #[test]
    fn gated_presses_motion_and_cursor_sync_are_never_replayed() {
        let link = ready_link(1);
        let view = link.capture_view();
        link.hold_for_monitor();
        assert!(
            view.send(1, vec![0, 0x42, 0]),
            "a new press is consumed, not replayed"
        );
        assert!(view.send(1, input_wire::motion(5, 3, 4, 0, 0).to_vec()));
        assert!(view.send(1, input_wire::absolute(6, 10, 20).to_vec()));
        assert!(view.send(1, input_wire::buttons_at(7, 1, 2, 1).to_vec()));
        assert!(
            !view.send(1, input_wire::cursor_sync(8).to_vec()),
            "cursor sync is retried after resume"
        );
        link.set_ready(true);
        assert!(link.next().is_none(), "no gated packet reaches the wire");
    }

    #[test]
    fn a_gated_release_owes_one_release_all_after_input_queued_before_the_gate() {
        let link = ready_link(1);
        let view = link.capture_view();
        assert!(
            view.send(1, vec![0, 0x41, 0]),
            "A down, queued before the gate"
        );
        assert!(view.send(1, input_wire::motion(9, 1, 1, 0, 0).to_vec()));
        link.hold_for_monitor();
        assert!(
            view.send(1, vec![1, 0x41, 0]),
            "A up is gated and owes a release"
        );
        assert!(view.send(1, vec![1, 0x41, 0]), "a repeat owes nothing more");
        assert!(view.send(1, vec![4, 0]));
        link.set_ready(true);
        assert_eq!(
            link.next(),
            Some(vec![0, 0x41, 0]),
            "earlier input goes first"
        );
        assert_eq!(link.next(), Some(vec![6]));
        assert!(
            link.next().is_none(),
            "queued motion was dropped at the gate, and only one release is sent"
        );
    }

    #[test]
    fn a_gated_scan_code_release_owes_a_release() {
        let link = ready_link(1);
        let view = link.capture_view();
        let down: [u8; 6] = [13, 0, 0x1e, 0, 0x41, 0];
        let up: [u8; 6] = [13, 1, 0x1e, 0, 0x41, 0];
        assert!(view.send(1, down.to_vec()));
        assert_eq!(link.next(), Some(down.to_vec()));
        link.hold_for_monitor();
        assert!(view.send(1, up.to_vec()));
        link.set_ready(true);
        assert_eq!(link.next(), Some(vec![6]));
        assert!(link.next().is_none());
    }

    #[test]
    fn the_transport_gate_is_closed_while_the_view_stays_ready_and_only_after_admission() {
        let link = InputLink::default();
        let view = link.capture_view();
        link.begin_epoch(1);
        link.hold_for_monitor();
        assert!(
            !view.snapshot().ready,
            "nothing to carry before the first admission"
        );
        assert_eq!(link.snapshot().gate_closes, 0);
        link.set_ready(true);
        assert_eq!(link.next(), Some(vec![6]));
        link.hold_for_monitor();
        link.hold_for_monitor();
        assert_eq!(
            link.snapshot().gate_closes,
            1,
            "a second hold is not a new closure"
        );
        assert!(!link.snapshot().ready);
        assert!(!link.ready_signal().load(Ordering::Acquire));
        assert!(view.snapshot().ready);
        assert!(view.ready_signal().load(Ordering::Acquire));
    }

    #[test]
    fn source_change_dead_transport_and_epoch_loss_release_and_end_the_carry() {
        // A closure during the carry releases on the next open, and counts once.
        let link = ready_link(1);
        let view = link.capture_view();
        assert!(view.send(1, vec![0, 0x41, 0]));
        assert_eq!(link.next(), Some(vec![0, 0x41, 0]));
        link.hold_for_monitor();
        link.set_ready(false);
        assert!(!view.snapshot().ready);
        assert!(!view.ready_signal().load(Ordering::Acquire));
        assert_eq!(
            link.snapshot().gate_closes,
            1,
            "the carry already counted the closure"
        );
        link.set_ready(true);
        assert_eq!(link.next(), Some(vec![6]));
        assert!(link.next().is_none());

        // An explicit release keeps the carry and releases the host on resume.
        let link = ready_link(1);
        let view = link.capture_view();
        assert!(view.send(1, vec![0, 0x41, 0]));
        assert_eq!(link.next(), Some(vec![0, 0x41, 0]));
        link.hold_for_monitor();
        link.release(1);
        assert!(
            view.snapshot().ready,
            "the carry continues through an explicit release"
        );
        assert!(view.send(1, vec![0, 0x42, 0]), "a gated press is consumed");
        link.set_ready(true);
        assert_eq!(link.next(), Some(vec![6]));
        assert!(link.next().is_none());

        // Epoch loss ends the carry at once and refuses the old epoch.
        let link = ready_link(1);
        let view = link.capture_view();
        assert!(view.send(1, vec![0, 0x41, 0]));
        assert_eq!(link.next(), Some(vec![0, 0x41, 0]));
        link.hold_for_monitor();
        link.begin_epoch(2);
        assert!(!view.snapshot().ready);
        assert!(!view.ready_signal().load(Ordering::Acquire));
        assert!(!view.send(1, vec![0, 0x42, 0]), "the old epoch is refused");
        link.set_ready(true);
        assert_eq!(link.next(), Some(vec![6]));
        assert!(link.next().is_none());
    }

    #[test]
    fn capture_view_numbers_sequence_from_the_mailbox_with_no_hole_at_a_gate() {
        let link = ready_link(1);
        let view = link.capture_view();
        assert!(
            view.send(1, input_wire::cursor_sync(500).to_vec()),
            "the producer's local number is ignored"
        );
        assert_eq!(link.next(), Some(input_wire::cursor_sync(0).to_vec()));
        assert!(view.send(1, input_wire::motion(40, 1, 1, 0, 0).to_vec()));
        assert_eq!(
            link.next(),
            Some(input_wire::motion(1, 1, 1, 0, 0).to_vec())
        );
        link.hold_for_monitor();
        assert!(view.send(1, input_wire::motion(41, 9, 9, 0, 0).to_vec()));
        assert!(view.send(1, input_wire::absolute(42, 3, 4).to_vec()));
        link.set_ready(true);
        assert_eq!(link.next(), Some(input_wire::cursor_sync(2).to_vec()));
        assert!(view.send(1, input_wire::absolute(43, 3, 4).to_vec()));
        assert_eq!(
            link.next(),
            Some(input_wire::absolute(3, 3, 4).to_vec()),
            "only the resume sync spent a wire number"
        );
    }

    #[test]
    fn work_monitor_resume_sync_follows_owed_release_and_replacement_clears_it() {
        let link = ready_link(1);
        let view = link.capture_view();
        assert!(view.send(1, input_wire::cursor_sync(0).to_vec()));
        assert_eq!(link.next(), Some(input_wire::cursor_sync(0).to_vec()));
        assert!(view.send(1, vec![0, 0x41, 0]));
        assert_eq!(link.next(), Some(vec![0, 0x41, 0]));
        link.hold_for_monitor();
        assert!(view.send(1, vec![1, 0x41, 0]));
        link.set_ready(true);
        assert_eq!(link.next(), Some(vec![6]));
        assert_eq!(link.next(), Some(input_wire::cursor_sync(1).to_vec()));
        assert!(link.next().is_none());

        link.restart_producer();
        assert_eq!(link.next(), Some(vec![6]));
        link.hold_for_monitor();
        link.set_ready(true);
        assert!(
            link.next().is_none(),
            "a new Game capture has no Work cursor sync"
        );
    }

    #[test]
    fn capture_view_sequence_wraps_past_the_top_of_the_space() {
        let link = ready_link(1);
        let view = link.capture_view();
        link.inner.lock().unwrap().next_sequence = u16::MAX;
        assert!(view.send(1, input_wire::cursor_sync(0).to_vec()));
        assert!(view.send(1, input_wire::cursor_sync(0).to_vec()));
        assert_eq!(
            link.next(),
            Some(input_wire::cursor_sync(u16::MAX).to_vec())
        );
        assert_eq!(link.next(), Some(input_wire::cursor_sync(0).to_vec()));
    }
}
