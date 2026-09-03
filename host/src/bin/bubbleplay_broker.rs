use std::ffi::OsString;

use anyhow::{Context, Result};
use windows_service::{define_windows_service, service_dispatcher};

#[path = "../service_runtime.rs"]
mod service_runtime;

use service_runtime::ServiceRole;

const BROKER_SERVICE_NAME: &str = "BubblePlayBroker";

define_windows_service!(ffi_service_main, service_main);

fn service_main(arguments: Vec<OsString>) {
    if let Err(error) =
        service_runtime::run_service(BROKER_SERVICE_NAME, ServiceRole::Broker, arguments)
    {
        tracing::error!(?error, "BubblePlay Broker service stopped with an error");
    }
}

fn main() -> Result<()> {
    init_logging();
    let console = std::env::args_os().any(|argument| argument == "--console");
    let run_worker = std::env::args_os().any(|argument| argument == "--run-worker");
    if console {
        return service_runtime::run_console(ServiceRole::Broker, run_worker);
    }
    service_dispatcher::start(BROKER_SERVICE_NAME, ffi_service_main)
        .context("starting BubblePlay Broker service dispatcher")
}

fn init_logging() {
    let _ = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_target(false)
        .try_init();
}
