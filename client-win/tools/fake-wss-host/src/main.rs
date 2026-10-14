//! fake-wss-host: a loopback relay + host that serves recorded Annex-B streams
//! as BPV4 frames over WSS v4. The codec is chosen by the browser's
//! `media-control start`. See `--help`.
//!
//! Exit codes: 0 every check passed, 1 a check failed, 2 usage or startup error.

use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context};
use fake_wss_host::cli::{self, Args, USAGE};
use fake_wss_host::evidence;
use fake_wss_host::model::Source;
use fake_wss_host::report::{self, RunMeta, StreamInfo};
use fake_wss_host::server::{self, ServerConfig};
use fake_wss_host::stream::{Codec, Stream};

fn usage_error(message: &str) -> ExitCode {
    eprintln!("fake-wss-host: {message}\n\n{USAGE}");
    ExitCode::from(2)
}

/// Every recording on offer, in the order `start` will look for them.
struct Library {
    sources: Vec<Arc<dyn Source>>,
    fps: u32,
    bitrate_bps: u64,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let args = match cli::parse(&raw) {
        Ok(Some(args)) => args,
        Ok(None) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(message) => return usage_error(&message),
    };

    let library = match load_library(&args) {
        Ok(library) => library,
        Err(error) => {
            eprintln!("fake-wss-host: {error:#}");
            return ExitCode::from(2);
        }
    };

    let listener = match server::bind(SocketAddr::from(([127, 0, 0, 1], args.port))).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("fake-wss-host: {error:#}");
            return ExitCode::from(2);
        }
    };
    let bind = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_default();
    let streams: Vec<StreamInfo> = library
        .sources
        .iter()
        .map(|s| stream_info(s.as_ref()))
        .collect();
    let offered: Vec<&str> = streams.iter().map(|s| s.codec).collect();
    eprintln!(
        "fake-wss-host: ws://{bind}/ws  offering {} @{} fps, {} bps, {} s, injections {:?}",
        offered.join(" + "),
        library.fps,
        library.bitrate_bps,
        args.exit_after.as_secs_f64(),
        args.injections
    );

    let config = ServerConfig {
        sources: library.sources,
        fps: library.fps,
        bitrate_bps: library.bitrate_bps,
        injections: args.injections,
        exit_after: args.exit_after,
        drain: Duration::from_millis(args.drain_ms),
        max_connections: 8,
    };
    let snapshot = match server::serve(listener, config).await {
        Ok(snapshot) => snapshot,
        Err(error) => {
            eprintln!("fake-wss-host: {error:#}");
            return ExitCode::from(2);
        }
    };

    let meta = RunMeta {
        constraint: args.constraint.map(|c| c.wire_name()),
        streams,
        fps: library.fps,
        bitrate_bps: library.bitrate_bps,
        exit_after_s: args.exit_after.as_secs_f64(),
        drain_ms: args.drain_ms,
        bind,
        injections: args.injections,
    };
    let document = report::build(&meta, &snapshot);
    let text = match serde_json::to_string_pretty(&document) {
        Ok(text) => text,
        Err(error) => return usage_error(&format!("cannot encode the report: {error}")),
    };
    println!("{text}");
    if let Some(path) = &args.stats {
        if let Err(error) = std::fs::write(path, format!("{text}\n")) {
            eprintln!("fake-wss-host: cannot write {}: {error}", path.display());
            return ExitCode::from(2);
        }
    }
    if document["pass"] == serde_json::Value::Bool(true) {
        ExitCode::SUCCESS
    } else {
        eprintln!("fake-wss-host: a check failed; see \"checks\" in the report");
        ExitCode::from(1)
    }
}

fn stream_info(source: &dyn Source) -> StreamInfo {
    StreamInfo {
        codec: source.codec().wire_name(),
        width: source.width(),
        height: source.height(),
    }
}

/// Load each recording on offer. Without `--stream` a recording that is not on
/// disk is skipped; with `--stream` it is required. A recording that is present
/// but fails verification is always an error, so a bad file is never served.
fn load_library(args: &Args) -> anyhow::Result<Library> {
    let wanted: Vec<Codec> = match args.constraint {
        Some(codec) => vec![codec],
        None => vec![Codec::H264, Codec::Hevc],
    };
    let mut loaded = Vec::new();
    for codec in wanted {
        let path = args.streams_dir.join(codec.file_name());
        if args.constraint.is_none() && !path.exists() {
            eprintln!(
                "fake-wss-host: no {} recording at {}, not offered",
                codec.wire_name(),
                path.display()
            );
            continue;
        }
        let evidence = evidence::load(&args.streams_dir, codec)?;
        let stream =
            Stream::load(codec, &path).with_context(|| format!("load {}", path.display()))?;
        evidence::verify(&stream, &evidence)?;
        loaded.push((evidence, Arc::new(stream)));
    }
    let Some((first, _)) = loaded.first() else {
        bail!("no recordings to serve in {}", args.streams_dir.display());
    };
    let (fps, bitrate_bps) = (first.fps, first.bitrate_bps);
    for (evidence, _) in &loaded {
        if evidence.fps != fps || evidence.bitrate_bps != bitrate_bps {
            bail!(
                "recordings disagree: {} is {} fps at {} bps, the first is {fps} fps at {bitrate_bps} bps",
                evidence.file,
                evidence.fps,
                evidence.bitrate_bps
            );
        }
    }
    let sources = loaded
        .into_iter()
        .map(|(_, stream)| stream as Arc<dyn Source>)
        .collect();
    Ok(Library {
        sources,
        fps,
        bitrate_bps,
    })
}
