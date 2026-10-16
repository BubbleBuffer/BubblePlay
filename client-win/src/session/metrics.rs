//! CPU timing at successful new-frame blits; no scanout or input-to-photon claim.
use crate::{media::StatsSnapshot, net::InputSnapshot};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Instant,
};
const SAMPLES: usize = 8192;
pub fn write_summary(path: &std::path::Path, value: &serde_json::Value) -> anyhow::Result<()> {
    std::fs::write(path, format!("{}\n", serde_json::to_string_pretty(value)?))?;
    Ok(())
}
#[derive(Default)]
struct Inner {
    last: Option<Instant>,
    intervals: VecDeque<f64>,
    receive_presented: VecDeque<f64>,
    frozen: bool,
}
#[derive(Clone, Default)]
pub struct PresentationMetrics(Arc<Mutex<Inner>>);
fn push(samples: &mut VecDeque<f64>, value: f64) {
    if samples.len() == SAMPLES {
        samples.pop_front();
    }
    samples.push_back(value);
}
pub(crate) fn percentile(values: impl IntoIterator<Item = f64>, percent: f64) -> Option<f64> {
    let mut values: Vec<_> = values.into_iter().filter(|v| v.is_finite()).collect();
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let index = ((percent * values.len() as f64).ceil() as usize)
        .saturating_sub(1)
        .min(values.len() - 1);
    Some(values[index])
}
impl PresentationMetrics {
    pub fn freeze(&self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).frozen = true;
    }
    pub fn presented(&self, received: Instant, at: Instant) {
        let mut inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if inner.frozen {
            return;
        }
        if let Some(last) = inner.last {
            push(
                &mut inner.intervals,
                at.saturating_duration_since(last).as_secs_f64() * 1000.0,
            );
        }
        inner.last = Some(at);
        push(
            &mut inner.receive_presented,
            at.saturating_duration_since(received).as_secs_f64() * 1000.0,
        );
    }
    pub fn summary(
        &self,
        stats: &StatsSnapshot,
        input: &InputSnapshot,
        errors: &[String],
    ) -> serde_json::Value {
        let inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let interval = |p| percentile(inner.intervals.iter().copied(), p);
        let latency = |p| percentile(inner.receive_presented.iter().copied(), p);
        serde_json::json!({
            "received_fps":stats.received_fps,"decoded_fps":stats.decoded_fps,"presented_fps":stats.presented_fps,
            "fps_window":"last one second, frozen before shell teardown",
            "frame_interval_ms":{"p50":interval(0.50),"p95":interval(0.95),"p99":interval(0.99),"samples":inner.intervals.len()},
            "receive_to_presented_ms":{"p50":latency(0.50),"p95":latency(0.95),"samples":inner.receive_presented.len()},
            "timing_method":"network receipt Instant to successful new-frame video-processor blit callback; intervals between those callbacks include stalls/reconnect gaps; most recent 8192 samples; CPU boundaries, not display scanout",
            "keyframe_requests":stats.totals.keyframe_requests,"drops":stats.totals.dropped,
            "errors":{"decode":stats.totals.decode_errors,"present":stats.totals.present_errors,"session":errors},
            "codec":stats.codec.map(|c|c.name()),"resolution":{"width":stats.width,"height":stats.height},
            "input_round_trip_ms":{"p50":input.rtt_p50_ms,"p95":input.rtt_p95_ms,"samples":input.rtt_samples,
                "method":"Work type-8 absolute packet send start to receipt of type-128 ACK_VALID with the exact same u16 sequence in the same epoch; includes host input processing/cursor publication and network return; excludes UI queueing and display latency; not measurable until a matching host echo arrives"}
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    #[test]
    fn percentiles_are_nearest_rank_and_empty_is_unmeasurable() {
        assert_eq!(percentile([], 0.5), None);
        assert_eq!(percentile([40., 10., 30., 20.], 0.5), Some(20.));
        assert_eq!(percentile([40., 10., 30., 20.], 0.95), Some(40.));
    }
    #[test]
    fn timing_samples_use_receipt_and_new_frame_callback_boundaries() {
        let metrics = PresentationMetrics::default();
        let origin = Instant::now();
        metrics.presented(origin, origin + Duration::from_millis(7));
        metrics.presented(
            origin + Duration::from_millis(10),
            origin + Duration::from_millis(25),
        );
        let summary = metrics.summary(&StatsSnapshot::default(), &InputSnapshot::default(), &[]);
        assert_eq!(summary["frame_interval_ms"]["p50"], 18.0);
        assert_eq!(summary["receive_to_presented_ms"]["p95"], 15.0);
        assert!(summary["input_round_trip_ms"]["p50"].is_null());
    }
}
