//! Epoch-scoped input mailbox. Only the /ws worker touches the socket.
//! UI producers never wait for network I/O; a full reliable queue releases
//! the remote state before accepting more input. Motion is coalesced by the
//! capture before admission, and the queue has the browser's 4 MiB limit.
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use tokio::sync::Notify;

const MAX_BYTES: usize = 4 * 1024 * 1024;

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
}

#[derive(Clone, Default)]
pub struct InputLink {
    inner: Arc<Mutex<Inner>>,
    pub(crate) notify: Arc<Notify>,
    wake: Option<Arc<dyn Fn() + Send + Sync>>,
    ready_signal: Arc<AtomicBool>,
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
            state.queue.clear();
            state.bytes = 0;
            state.needs_release = true;
        }
    }
    pub fn ready_signal(&self) -> Arc<AtomicBool> {
        self.ready_signal.clone()
    }
    pub fn new(wake: Arc<dyn Fn() + Send + Sync>) -> Self {
        Self {
            wake: Some(wake),
            ..Self::default()
        }
    }
    pub fn snapshot(&self) -> InputSnapshot {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .view
            .clone()
    }
    fn wake_ui(&self) {
        if let Some(wake) = &self.wake {
            wake();
        }
    }
    pub(crate) fn begin_epoch(&self, epoch: u64) {
        self.ready_signal.store(false, Ordering::Release);
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let (sent, refused, releases) = (state.view.sent, state.view.refused, state.view.releases);
        *state = Inner {
            view: InputSnapshot {
                epoch,
                sent,
                refused,
                releases,
                ..Default::default()
            },
            needs_release: true,
            ..Default::default()
        };
        drop(state);
        self.wake_ui();
    }
    pub(crate) fn set_ready(&self, ready: bool) {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if state.view.ready == ready {
            return;
        }
        state.view.ready = ready;
        self.ready_signal.store(ready, Ordering::Release);
        if !ready {
            state.queue.clear();
            state.bytes = 0;
            state.needs_release = true;
        }
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
    pub fn send(&self, epoch: u64, packet: Vec<u8>) -> bool {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if !state.view.ready || epoch != state.view.epoch {
            state.view.refused += 1;
            return false;
        }
        if state.bytes + packet.len() > MAX_BYTES {
            state.queue.clear();
            state.bytes = 0;
            state.needs_release = true;
            state.view.refused += 1;
            drop(state);
            self.notify.notify_one();
            return false;
        }
        state.bytes += packet.len();
        state.queue.push_back(packet);
        drop(state);
        self.notify.notify_one();
        true
    }
    /// Release is a barrier: stale queued input is discarded. When the lane
    /// is gated/dead, retain the release for the first ready event on resume.
    pub fn release(&self, epoch: u64) {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if state.view.epoch != epoch {
            return;
        }
        state.queue.clear();
        state.bytes = 0;
        state.needs_release = true;
        drop(state);
        self.notify.notify_one();
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

#[cfg(test)]
mod tests {
    use super::*;
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
}
