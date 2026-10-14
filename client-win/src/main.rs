//! BubblePlay native Windows viewer.
//!
//! `bubbleplay-client --ui-demo` opens the real window against the mock
//! backend; F12 cycles through every UI state. Add `--shots <dir>` to save
//! one PNG per state and exit.

#![windows_subsystem = "windows"]

mod model;
mod mock;
mod shell;
mod ui;
mod net; mod media; mod session;

use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    if let Some(dev) = session::DevOptions::from_args(std::env::args().skip(1))? { return session::run_dev(dev); }
    let mut demo = false;
    let mut shots: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--ui-demo" => demo = true,
            "--shots" => shots = args.next().map(PathBuf::from),
            _ => {}
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
    let mut video = shell::PlaceholderVideo::default();
    shell.run(&mut app, &mut video)
}
