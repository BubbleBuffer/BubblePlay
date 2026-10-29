//! BubblePlay native Windows viewer.
//!
//! `bubbleplay-client --ui-demo` opens the real window against the mock
//! backend; F12 cycles through every UI state. Add `--shots <dir>` to save
//! one PNG per state and exit.

#![windows_subsystem = "windows"]

mod audio;
mod input;
mod media;
mod mock;
mod model;
mod net;
mod session;
mod shell;
mod store;
mod ui;

use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if store::cli::is_subcommand(&args) {
        // `key ...` / `pcs ...`: print to the console the command was started from.
        store::console::attach();
        if let Err(error) = store::cli::run(&args) {
            eprintln!("BubblePlay: {error:#}");
            std::process::exit(1);
        }
        return Ok(());
    }
    if let Some(dev) = session::DevOptions::from_args(std::env::args().skip(1))? {
        if let Err(error) = session::run_dev(dev) {
            if let Some(capability) = error.downcast_ref::<media::NoHardwareVideoDecoder>() {
                eprintln!("BubblePlay: {capability}");
            } else {
                eprintln!(
                    "BubblePlay: {}",
                    error.to_string().replace(['\r', '\n'], " ")
                );
            }
            std::process::exit(1);
        }
        return Ok(());
    }
    let mut demo = false;
    let mut shots: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--ui-demo" => demo = true,
            "--shots" => shots = args.next().map(PathBuf::from),
            _ => anyhow::bail!("unknown flag: {a}"),
        }
    }
    if !demo {
        // The real backend arrives with the net/media modules.
        anyhow::bail!("only --ui-demo is available in this build");
    }
    if let Some(dir) = &shots {
        std::fs::create_dir_all(dir)?;
    }

    let shell = shell::Shell::new(shell::ShellConfig {
        title: "BubblePlay".into(),
        size: (ui::theme::WINDOW_W, ui::theme::WINDOW_H),
        min_size: (760.0, 520.0),
    })?;

    let (mut backend, driver) = mock::new();
    let wake = shell.wake_handle();
    model::Backend::set_notifier(&mut backend, std::sync::Arc::new(move || wake.wake()));

    let mut app = ui::App::new(Box::new(backend)).with_demo(Box::new(driver), shots);
    let mut video = shell::PlaceholderVideo;
    shell.run(&mut app, &mut video)
}
