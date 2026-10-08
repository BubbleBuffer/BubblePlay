//! Scheduled task target. No implicit mode, dev bypass or permanent service.
#![windows_subsystem = "windows"]
#[allow(dead_code)]
#[path = "../tray/logic.rs"]
mod logic;
fn main() {
    let Ok(path) = bubbleplay_host::secure_helper::trust::log_path() else {
        std::process::exit(1);
    };
    let log = logic::SharedLog::new(path);
    let _ = tracing_subscriber::fmt()
        .with_writer(move || log.clone())
        .with_ansi(false)
        .with_env_filter(tracing_subscriber::EnvFilter::new("info"))
        .try_init();
    let args: Vec<_> = std::env::args().skip(1).collect();
    let result = match args.as_slice() {
        [mode] if mode == "--launch" => bubbleplay_host::secure_helper::runtime::launch(),
        [mode] if mode == "--serve" => bubbleplay_host::secure_helper::runtime::serve(),
        _ => Err(anyhow::anyhow!("expected --launch or --serve")),
    };
    if let Err(e) = result {
        tracing::error!(error=%e,"secure helper stopped");
        std::process::exit(1);
    }
}
