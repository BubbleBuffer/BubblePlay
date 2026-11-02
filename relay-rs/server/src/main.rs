//! `bp-relay`: the relay binary. See relay-rs/README.md for the subcommands.

use std::path::PathBuf;
use std::process::ExitCode;

use bp_relay_server::{config, health, import, init, log, server};
use clap::{Parser, Subcommand};

const DEFAULT_CONFIG: &str = "/data/relay.json";

#[derive(Parser)]
#[command(name = "bp-relay", version, about = "BubblePlay relay server")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the relay.
    Serve {
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
    },
    /// Write an example relay.json with fresh random secrets. Refuses to overwrite.
    Init { path: PathBuf },
    /// Check /healthz on the local listen address. Exits 0 when healthy.
    Healthcheck {
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
    },
    /// Copy the registered PCs of a Go relay auth.json into pcs.json.
    ImportGoAuth {
        auth: PathBuf,
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
    },
}

fn main() -> ExitCode {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let result = match cli.command {
        Command::Serve { config } => serve(&config),
        Command::Init { path } => init_command(&path),
        Command::Healthcheck { config } => health::healthcheck(&config).map(|()| {
            println!("ok");
        }),
        Command::ImportGoAuth { auth, config } => import_command(&auth, &config),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("bp-relay: {message}");
            ExitCode::FAILURE
        }
    }
}

fn serve(path: &std::path::Path) -> Result<(), String> {
    let cfg = config::load_config(path).map_err(|e| e.to_string())?;
    log::install(&cfg);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("cannot start the async runtime: {e}"))?;
    runtime
        .block_on(server::serve(&cfg))
        .map_err(|e| e.to_string())
}

fn init_command(path: &std::path::Path) -> Result<(), String> {
    match init::write_example_config(path) {
        Ok(()) => {
            println!("wrote {}", path.display());
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(format!("refusing to overwrite {}", path.display()))
        }
        Err(error) => Err(format!("cannot write {}: {error}", path.display())),
    }
}

fn import_command(auth: &std::path::Path, config_path: &std::path::Path) -> Result<(), String> {
    let cfg = config::load_config(config_path).map_err(|e| e.to_string())?;
    let report = import::import_go_auth_file(&cfg.state_dir, auth).map_err(|e| e.to_string())?;
    println!(
        "imported {}, already present {}, skipped {}",
        report.imported,
        report.already_present,
        report.skipped.len()
    );
    for reason in &report.skipped {
        println!("skipped {reason}");
    }
    Ok(())
}
