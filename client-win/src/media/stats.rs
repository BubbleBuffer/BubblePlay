//! Polling statistics for the media path. Thread-safe and COM-free, so the
//! session and the UI can both record and read without owning the pipeline.
//! Rates and percentiles cover the last one-second window; totals are cumulative.

use super::backend::Codec;
use std::{
    collections::{HashMap, VecDeque},
    fmt,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

const WINDOW: Duration = Duration::from_secs(1);
/// Receive times older than this without a matching decode are forgotten.
const PENDING_TTL: Duration = Duration::from_secs(5);
const PENDING_MAX: usize = 1024;

/// Cumulative counters since the stats object was created.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Totals {
    pub received: u64,
    pub decoded: u64,
    pub presented: u64,
    /// Decoded frames replaced in the mailbox before the UI drew them.
    pub dropped: u64,
    pub keyframe_requests: u64,
    pub decode_errors: u64,
    pub present_errors: u64,
}

/// One-second window plus totals, read with [`MediaStats::snapshot`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StatsSnapshot {
    pub received_fps: f64,
    pub decoded_fps: f64,
    pub presented_fps: f64,
    pub recv_to_decode_p50_ms: Option<f64>,
    pub recv_to_decode_p95_ms: Option<f64>,
    pub decode_to_present_p50_ms: Option<f64>,
    pub decode_to_present_p95_ms: Option<f64>,
    pub drops_1s: u32,
    pub keyframe_requests_1s: u32,
    pub codec: Option<Codec>,
    pub width: u32,
    pub height: u32,
    pub totals: Totals,
}

impl fmt::Display for StatsSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let codec = self.codec.map_or("none", Codec::name);
        let ms = |v: Option<f64>| v.map_or("n/a".to_owned(), |v| format!("{v:.2}"));
        write!(
            f,
            "codec={codec} {}x{} | fps recv={:.1} decode={:.1} present={:.1} | \
             recv->decode p50={} p95={} ms | decode->present p50={} p95={} ms | \
             drops/1s={} keyframe_req/1s={} | total recv={} decoded={} presented={} \
             dropped={} keyframe_req={} decode_errors={} present_errors={}",
            self.width,
            self.height,
            self.received_fps,
            self.decoded_fps,
            self.presented_fps,
            ms(self.recv_to_decode_p50_ms),
            ms(self.recv_to_decode_p95_ms),
            ms(self.decode_to_present_p50_ms),
            ms(self.decode_to_present_p95_ms),
            self.drops_1s,
            self.keyframe_requests_1s,
            self.totals.received,
            self.totals.decoded,
            self.totals.presented,
            self.totals.dropped,
            self.totals.keyframe_requests,
            self.totals.decode_errors,
            self.totals.present_errors,
        )
    }
}

#[derive(Default)]
struct Inner {
    received: VecDeque<Instant>,
    decoded: VecDeque<Instant>,
    presented: VecDeque<Instant>,
    drops: VecDeque<Instant>,
    keyframe_requests: VecDeque<Instant>,
    recv_to_decode: VecDeque<(Instant, f64)>,
    decode_to_present: VecDeque<(Instant, f64)>,
    /// Receive time per PTS, waiting for its decode completion.
    pending: HashMap<i64, Instant>,
    totals: Totals,
    format: Option<(Codec, u32, u32)>,
}

fn prune(queue: &mut VecDeque<Instant>, now: Instant) {
    while queue
        .front()
        .is_some_and(|t| now.duration_since(*t) > WINDOW)
    {
        queue.pop_front();
    }
}

fn prune_samples(queue: &mut VecDeque<(Instant, f64)>, now: Instant) {
    while queue
        .front()
        .is_some_and(|(t, _)| now.duration_since(*t) > WINDOW)
    {
        queue.pop_front();
    }
}

/// Nearest-rank percentile over a set of samples.
fn percentile(mut values: Vec<f64>, p: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| a.total_cmp(b));
    let rank = (p * values.len() as f64).ceil() as usize;
    Some(values[rank.clamp(1, values.len()) - 1])
}

impl Inner {
    fn prune_pending(&mut self, now: Instant) {
        self.pending
            .retain(|_, received| now.duration_since(*received) <= PENDING_TTL);
        if self.pending.len() > PENDING_MAX {
            self.pending.clear();
        }
    }
}

/// Shared, cloneable statistics handle.
#[derive(Clone, Default)]
pub struct MediaStats {
    inner: Arc<Mutex<Inner>>,
}

impl MediaStats {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// An access unit arrived from the transport. `at` is its receive time.
    pub fn note_received(&self, pts_100ns: i64, at: Instant) {
        let mut s = self.lock();
        s.totals.received += 1;
        s.received.push_back(at);
        prune(&mut s.received, at);
        s.prune_pending(at);
        s.pending.insert(pts_100ns, at);
    }

    /// The session asked the sender for a keyframe.
    #[cfg(test)]
    pub fn note_keyframe_request(&self, at: Instant) {
        let mut s = self.lock();
        s.totals.keyframe_requests += 1;
        s.keyframe_requests.push_back(at);
        prune(&mut s.keyframe_requests, at);
    }

    /// Decoder returned an error for an access unit or a surface.
    pub fn note_decode_error(&self) {
        self.lock().totals.decode_errors += 1;
    }

    /// Drawing a frame failed. The frame is not counted as presented.
    pub fn note_present_error(&self) {
        self.lock().totals.present_errors += 1;
    }

    /// A decoded picture reached the mailbox at `at`.
    pub fn note_decoded(&self, pts_100ns: i64, at: Instant) {
        let mut s = self.lock();
        s.totals.decoded += 1;
        s.decoded.push_back(at);
        prune(&mut s.decoded, at);
        if let Some(received) = s.pending.remove(&pts_100ns) {
            let ms = at.duration_since(received).as_secs_f64() * 1000.0;
            s.recv_to_decode.push_back((at, ms));
            prune_samples(&mut s.recv_to_decode, at);
        }
    }

    /// The mailbox replaced an undrawn frame.
    pub fn note_dropped(&self, at: Instant) {
        let mut s = self.lock();
        s.totals.dropped += 1;
        s.drops.push_back(at);
        prune(&mut s.drops, at);
    }

    /// A new decoded frame was first drawn at `at`.
    pub fn note_presented(&self, decoded_at: Instant, at: Instant) {
        let mut s = self.lock();
        s.totals.presented += 1;
        s.presented.push_back(at);
        prune(&mut s.presented, at);
        let ms = at.duration_since(decoded_at).as_secs_f64() * 1000.0;
        s.decode_to_present.push_back((at, ms));
        prune_samples(&mut s.decode_to_present, at);
    }

    /// Records the stream format that is currently decoding.
    pub fn set_format(&self, codec: Codec, width: u32, height: u32) {
        self.lock().format = Some((codec, width, height));
    }

    /// Reads the one-second window ending at `now`, plus the totals.
    pub fn snapshot(&self, now: Instant) -> StatsSnapshot {
        let mut guard = self.lock();
        let s = &mut *guard;
        for queue in [
            &mut s.received,
            &mut s.decoded,
            &mut s.presented,
            &mut s.drops,
            &mut s.keyframe_requests,
        ] {
            prune(queue, now);
        }
        prune_samples(&mut s.recv_to_decode, now);
        prune_samples(&mut s.decode_to_present, now);
        let recv_ms: Vec<f64> = s.recv_to_decode.iter().map(|(_, ms)| *ms).collect();
        let present_ms: Vec<f64> = s.decode_to_present.iter().map(|(_, ms)| *ms).collect();
        let (codec, width, height) = match s.format {
            Some((codec, w, h)) => (Some(codec), w, h),
            None => (None, 0, 0),
        };
        StatsSnapshot {
            received_fps: s.received.len() as f64,
            decoded_fps: s.decoded.len() as f64,
            presented_fps: s.presented.len() as f64,
            recv_to_decode_p50_ms: percentile(recv_ms.clone(), 0.50),
            recv_to_decode_p95_ms: percentile(recv_ms, 0.95),
            decode_to_present_p50_ms: percentile(present_ms.clone(), 0.50),
            decode_to_present_p95_ms: percentile(present_ms, 0.95),
            drops_1s: s.drops.len() as u32,
            keyframe_requests_1s: s.keyframe_requests.len() as u32,
            codec,
            width,
            height,
            totals: s.totals,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_nearest_rank() {
        let v = vec![1.0, 2.0, 3.0, 4.0];
        assert_eq!(percentile(v.clone(), 0.50), Some(2.0));
        assert_eq!(percentile(v.clone(), 0.95), Some(4.0));
        assert_eq!(percentile(Vec::new(), 0.5), None);
    }

    #[test]
    fn window_and_totals() {
        let stats = MediaStats::new();
        let t0 = Instant::now();
        stats.note_received(10, t0);
        stats.note_decoded(10, t0 + Duration::from_millis(4));
        stats.note_presented(t0 + Duration::from_millis(4), t0 + Duration::from_millis(9));
        stats.note_dropped(t0);
        let s = stats.snapshot(t0 + Duration::from_millis(10));
        assert_eq!(s.received_fps, 1.0);
        assert_eq!(s.presented_fps, 1.0);
        assert_eq!(s.drops_1s, 1);
        assert_eq!(s.totals.received, 1);
        assert!((s.recv_to_decode_p50_ms.unwrap() - 4.0).abs() < 0.01);
        assert!((s.decode_to_present_p50_ms.unwrap() - 5.0).abs() < 0.01);
        let later = stats.snapshot(t0 + Duration::from_secs(2));
        assert_eq!(later.received_fps, 0.0);
        assert_eq!(later.totals.received, 1);
    }

    #[test]
    fn unpolled_windows_remain_bounded_and_keep_totals() {
        let stats = MediaStats::new();
        let start = Instant::now();
        for n in 0..600 {
            let at = start + Duration::from_millis(n * 17);
            stats.note_received(n as i64, at);
            stats.note_decoded(n as i64, at + Duration::from_millis(2));
            stats.note_presented(at + Duration::from_millis(2), at + Duration::from_millis(4));
            stats.note_dropped(at);
            stats.note_keyframe_request(at);
        }
        let inner = stats.lock();
        for count in [
            inner.received.len(),
            inner.decoded.len(),
            inner.presented.len(),
            inner.drops.len(),
            inner.keyframe_requests.len(),
            inner.recv_to_decode.len(),
            inner.decode_to_present.len(),
        ] {
            assert!(
                count <= 60,
                "an unpolled one-second window retained {count} samples"
            );
        }
        assert_eq!(inner.totals.decoded, 600);
        assert_eq!(inner.totals.presented, 600);
    }
}
