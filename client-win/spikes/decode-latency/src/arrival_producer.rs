//! Lean companion: only shared arrival protocol and native timer policy. No
//! COM/MF, FFmpeg, D3D device, window, or presenter initialization occurs here.
#[allow(dead_code)]
mod pacing;
#[allow(dead_code)]
mod timing;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if !pacing::maybe_run_producer(&args)? {
        anyhow::bail!("arrival-producer.exe requires the spike's --arrival-producer arguments");
    }
    Ok(())
}
