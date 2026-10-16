use crate::presenter::PresentRecord;
use anyhow::Result;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
};
pub struct Row {
    pub index: usize,
    pub scheduled: i64,
    pub arrived: i64,
    pub submit: i64,
    pub output: i64,
    pub decoded: i64,
    pub present: PresentRecord,
}
pub fn percentile(v: &[f64], p: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    s[((s.len() as f64 * p).ceil() as usize)
        .saturating_sub(1)
        .min(s.len() - 1)]
}
fn stddev(v: &[f64]) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    let mean = v.iter().sum::<f64>() / v.len() as f64;
    (v.iter().map(|n| (n - mean).powi(2)).sum::<f64>() / v.len() as f64).sqrt()
}
pub fn write_results(
    dir: &Path,
    tag: &str,
    codec: &str,
    backend: &str,
    mode: &str,
    rows: &[Row],
    expected: usize,
    freq: i64,
    description: &str,
    wait_timeouts: u64,
    gpu_polls: u64,
    error: Option<&str>,
) -> Result<String> {
    fs::create_dir_all(dir)?;
    let mut csv = fs::File::create(dir.join(format!("{tag}.csv")))?;
    writeln!(csv,"frame,scheduled_qpc,t_submit_qpc,t_output_qpc,t_decoded_qpc,t_presented_qpc,submit_decoded_ms,decoded_presented_ms,submit_presented_ms,pacing_late_ms,present_id,stats_present_count,present_refresh_count,sync_refresh_count,sync_qpc,stats_hresult,media_present_count,media_sync_qpc,composition_mode,t_arrived_qpc,arrival_late_ms,arrival_submit_ms,arrival_presented_ms")?;
    let ms = |n: i64| n as f64 * 1000.0 / freq as f64;
    for r in rows {
        let stats = r
            .present
            .stats
            .map(|s| {
                format!(
                    "{},{},{},{}",
                    s.PresentCount, s.PresentRefreshCount, s.SyncRefreshCount, s.SyncQPCTime
                )
            })
            .unwrap_or_else(|| ",,,".into());
        let media = r
            .present
            .media
            .map(|s| {
                format!(
                    "{},{},{}",
                    s.PresentCount, s.SyncQPCTime, s.CompositionMode.0
                )
            })
            .unwrap_or_else(|| ",,".into());
        writeln!(
            csv,
            "{},{},{},{},{},{},{:.6},{:.6},{:.6},{:.6},{},{},{},{media},{},{:.6},{:.6},{:.6}",
            r.index,
            r.scheduled,
            r.submit,
            r.output,
            r.decoded,
            r.present.qpc,
            ms(r.decoded - r.submit),
            ms(r.present.qpc - r.decoded),
            ms(r.present.qpc - r.submit),
            ms(r.submit - r.scheduled),
            r.present.present_id,
            stats,
            r.present
                .stats_error
                .map(|e| format!("0x{:08x}", e as u32))
                .unwrap_or_default(),
            r.arrived,
            ms(r.arrived - r.scheduled),
            ms(r.submit - r.arrived),
            ms(r.present.qpc - r.arrived)
        )?;
    }
    let sd: Vec<f64> = rows.iter().map(|r| ms(r.decoded - r.submit)).collect();
    let dp: Vec<f64> = rows.iter().map(|r| ms(r.present.qpc - r.decoded)).collect();
    let sp: Vec<f64> = rows.iter().map(|r| ms(r.present.qpc - r.submit)).collect();
    let intervals: Vec<f64> = rows
        .windows(2)
        .map(|r| ms(r[1].present.qpc - r[0].present.qpc))
        .collect();
    let late_submit = rows
        .iter()
        .filter(|r| ms(r.submit - r.scheduled) > 1.0)
        .count();
    let late_frame = rows
        .iter()
        .filter(|r| ms(r.present.qpc - r.scheduled) > 1000.0 / 60.0)
        .count();
    let observed_stats = rows.iter().filter(|r| r.present.stats.is_some()).count();
    let dropped = expected.saturating_sub(rows.len());
    let values = [&sd, &dp, &sp]
        .iter()
        .map(|v| {
            format!(
                "{:.3},{:.3},{:.3},{:.3}",
                percentile(v, 0.5),
                percentile(v, 0.95),
                percentile(v, 0.99),
                percentile(v, 1.0)
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let summary=format!("{tag},{backend},{codec},{mode},{expected},{},{dropped},{late_submit},{late_frame},{values},{:.3},{:.3},{:.3},{observed_stats},{wait_timeouts},{gpu_polls},{}",rows.len(),stddev(&intervals),percentile(&intervals,0.99),percentile(&intervals,0.5),if error.is_some(){"failed"}else{"ok"});
    let path = dir.join("summary-v2.csv");
    let new = !path.exists();
    let mut out = OpenOptions::new().append(true).create(true).open(path)?;
    if new {
        writeln!(out,"run,backend,codec,present,expected,presented,app_dropped,submit_late_over_1ms,frame_late_over_period,sd_median,sd_p95,sd_p99,sd_max,dp_median,dp_p95,dp_p99,dp_max,sp_median,sp_p95,sp_p99,sp_max,interval_stddev,interval_p99,interval_median,stats_available,wait_timeout_polls,gpu_query_polls,status")?;
    }
    writeln!(out, "{summary}")?;
    let mut info = fs::File::create(dir.join(format!("{tag}.txt")))?;
    writeln!(info,"backend={description}\nbackend_name={backend}\nQPC_frequency={freq}\nframes_expected={expected}\nframes_presented={}\napplication_dropped={dropped}\nsubmit_late_threshold_ms=1\nframe_late_threshold_ms=16.666667\nwait_timeout_polls={wait_timeouts}\ngpu_query_polls={gpu_polls}\nerror={}\nDecode completion observes shared immediate-context ID3D11Fence Signal + SetEventOnCompletion + kernel wait; no timed probe copy or GPU query polling. Present endpoint is CPU return, not optical or actual flip. All frames included, no warmup exclusion. DXGI raw stats may refer to earlier presents; SyncQPCTime is a reference, not per-frame flip time. Display drops are unknown.\n",rows.len(),error.unwrap_or("none"))?;
    if rows.len() > 120 {
        let steady: Vec<f64> = rows
            .iter()
            .skip(120)
            .map(|r| ms(r.decoded - r.submit))
            .collect();
        writeln!(
            info,
            "steady_after_120_sd_median_ms={:.3}\nsteady_after_120_sd_p99_ms={:.3}",
            percentile(&steady, 0.5),
            percentile(&steady, 0.99)
        )?;
    }
    let arrivals: Vec<f64> = rows.iter().map(|r| ms(r.arrived - r.scheduled)).collect();
    let receive_present: Vec<f64> = rows.iter().map(|r| ms(r.present.qpc - r.arrived)).collect();
    writeln!(info,"independent_60fps_arrival_producer=true\narrival_late_over_1ms={}\narrival_lateness_median_ms={:.3}\narrival_lateness_p99_ms={:.3}\narrival_lateness_max_ms={:.3}\narrival_presented_median_ms={:.3}\narrival_presented_p99_ms={:.3}\narrival_presented_max_ms={:.3}",arrivals.iter().filter(|v|**v>1.0).count(),percentile(&arrivals,0.5),percentile(&arrivals,0.99),percentile(&arrivals,1.0),percentile(&receive_present,0.5),percentile(&receive_present,0.99),percentile(&receive_present,1.0))?;
    Ok(summary)
}
