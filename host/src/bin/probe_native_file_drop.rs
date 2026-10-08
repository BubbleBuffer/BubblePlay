//! Explicit real-desktop probe using owned fixture files only.
use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::POINT;
fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let x: i32 = args
        .next()
        .context("usage: probe_native_file_drop <screen-x> <screen-y> <owned-fixture>...")?
        .parse()?;
    let y: i32 = args.next().context("screen-y required")?.parse()?;
    let files: Vec<PathBuf> = args.map(PathBuf::from).collect();
    if files.is_empty() {
        bail!("owned fixture paths required");
    }
    let probe = bubbleplay_host::native_file_drop::diagnostic_drop(&files, POINT { x, y })?;
    let started = Instant::now();
    loop {
        if let Some(result) = probe.result() {
            println!(
                "native-drop-result={} elapsed-ms={}",
                result.as_ref().map_or_else(|e| e.as_str(), |_| "accepted"),
                started.elapsed().as_millis()
            );
            if let Err(error) = result {
                bail!(error);
            }
            // Allow async consumers to release their streams before process exit.
            std::thread::sleep(Duration::from_secs(2));
            return Ok(());
        }
        if started.elapsed() > Duration::from_secs(61 * 60) {
            probe.cancel();
            bail!("probe timeout");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
