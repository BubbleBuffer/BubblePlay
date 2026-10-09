use bubbleplay_host::agent_runtime;
use bubbleplay_host::privileged_backend;
use bubbleplay_host::privileged_ipc;
use bubbleplay_host::privileged_pipe;

use std::io::{self, Read, Write};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use agent_runtime::{
    spawn_agent_runtime, AgentRuntimeHandle, BoundaryBackend, WindowsInputDesktopSource,
};
use privileged_backend::RealAgentBackend;
use privileged_ipc::{read_frame, write_frame, Message};
use privileged_pipe::{PipeRole, PipeServer};

const SESSION_ERROR_LOG_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Default)]
struct ErrorLogGate {
    last_emitted: Option<Instant>,
    suppressed: u64,
}

impl ErrorLogGate {
    fn admit(&mut self, now: Instant) -> Option<u64> {
        if self
            .last_emitted
            .is_some_and(|last| now.saturating_duration_since(last) < SESSION_ERROR_LOG_INTERVAL)
        {
            self.suppressed = self.suppressed.saturating_add(1);
            return None;
        }
        self.last_emitted = Some(now);
        Some(std::mem::take(&mut self.suppressed))
    }
}

/// Development process boundary: framed BPIP control enters on stdin and
/// framed control/media events leave on stdout. The runtime still keeps
/// bounded control and media queues internally; a single writer serializes
/// them so frame bytes can never interleave. The Windows service will replace
/// only this stdio adapter with its two named-pipe adapters.
fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(io::stderr)
        .init();

    if std::env::args_os().any(|argument| argument == "--stdio") {
        return run_stdio();
    }
    let mut session_error_log = ErrorLogGate::default();
    loop {
        if let Err(error) = run_pipe_session() {
            if let Some(suppressed) = session_error_log.admit(Instant::now()) {
                tracing::warn!(
                    ?error,
                    suppressed,
                    "privileged Agent pipe session ended; rebinding"
                );
            }
            thread::sleep(Duration::from_millis(250));
        }
    }
}

fn run_stdio() -> Result<()> {
    let runtime = spawn_agent_runtime(BoundaryBackend::default(), WindowsInputDesktopSource);
    let input = runtime.control.clone();
    let (reader_done_tx, reader_done_rx) = mpsc::channel();
    let reader = thread::Builder::new()
        .name("bubbleplay-agent-stdio-reader".into())
        .spawn(move || {
            let result = read_commands(io::stdin().lock(), input);
            let _ = reader_done_tx.send(result.map_err(|error| error.to_string()));
        })
        .context("spawn stdin IPC reader")?;

    let mut stdout = io::BufWriter::new(io::stdout().lock());
    let mut output_sequence = 1u32;
    let mut control_open = true;
    let mut media_open = true;
    let mut reader_result = None;
    while control_open || media_open {
        if control_open {
            match runtime
                .control_events
                .recv_timeout(Duration::from_millis(10))
            {
                Ok(message) => write_output(&mut stdout, &mut output_sequence, &message)?,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => control_open = false,
            }
        }
        while media_open {
            match runtime.media_events.try_recv() {
                Ok(message) => write_output(&mut stdout, &mut output_sequence, &message)?,
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    media_open = false;
                    break;
                }
            }
        }
        if reader_result.is_none() {
            if let Ok(result) = reader_done_rx.try_recv() {
                reader_result = Some(result);
            }
        }
    }
    stdout.flush().context("flush agent IPC stdout")?;
    reader
        .join()
        .map_err(|_| anyhow::anyhow!("stdin IPC reader panicked"))?;
    runtime
        .join()
        .map_err(|_| anyhow::anyhow!("agent runtime panicked"))?;
    if let Some(Err(error)) = reader_result {
        anyhow::bail!("stdin IPC reader failed: {error}");
    }
    Ok(())
}

fn run_pipe_session() -> Result<()> {
    // Bind both unique names before accepting either connection so the
    // LocalService transport can connect control then media without a race.
    let control_server =
        PipeServer::bind_system(PipeRole::Control).context("bind privileged Agent control pipe")?;
    let media_server =
        PipeServer::bind_system(PipeRole::Media).context("bind privileged Agent media pipe")?;
    let mut control_writer = control_server
        .accept()
        .context("accept Agent control client")?;
    let mut media_writer = media_server.accept().context("accept Agent media client")?;
    anyhow::ensure!(
        control_writer.peer_process_id() == media_writer.peer_process_id(),
        "control and media pipes were opened by different processes"
    );
    let runtime = spawn_agent_runtime(RealAgentBackend::new(), WindowsInputDesktopSource);
    let result = pump_pipe_session(&runtime, &mut control_writer, &mut media_writer);
    let _ = runtime.control.send(Message::Shutdown);
    drop(control_writer);
    drop(media_writer);
    runtime
        .join()
        .map_err(|_| anyhow::anyhow!("agent runtime panicked"))?;
    result
}

fn pump_pipe_session(
    runtime: &AgentRuntimeHandle,
    control: &mut privileged_pipe::PipeStream,
    media: &mut privileged_pipe::PipeStream,
) -> Result<()> {
    let mut control_sequence = 1u32;
    let mut media_sequence = 1u32;
    loop {
        let mut progressed = false;
        loop {
            match runtime.control_events.try_recv() {
                Ok(message) => {
                    write_output(control, &mut control_sequence, &message)?;
                    progressed = true;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
            }
        }
        loop {
            match runtime.media_events.try_recv() {
                Ok(message) => {
                    write_output(media, &mut media_sequence, &message)?;
                    progressed = true;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
            }
        }

        match control
            .available_bytes()
            .context("poll Agent control pipe")?
        {
            0 => {}
            _ => {
                progressed = true;
                match read_frame(control).context("read agent IPC frame")? {
                    Some(frame) => runtime
                        .control
                        .send(frame.message)
                        .context("agent runtime disconnected")?,
                    None => return Ok(()),
                }
            }
        }

        if !progressed {
            thread::sleep(Duration::from_millis(1));
        }
    }
}

fn read_commands<R: Read>(mut input: R, control: mpsc::SyncSender<Message>) -> Result<()> {
    loop {
        match read_frame(&mut input).context("read agent IPC frame")? {
            Some(frame) => control
                .send(frame.message)
                .context("agent runtime disconnected")?,
            None => {
                let _ = control.send(Message::Shutdown);
                return Ok(());
            }
        }
    }
}

fn write_output<W: Write>(writer: &mut W, sequence: &mut u32, message: &Message) -> Result<()> {
    write_frame(writer, 0, *sequence, message).context("write agent IPC frame")?;
    writer.flush().context("flush agent IPC frame")?;
    *sequence = sequence.wrapping_add(1).max(1);
    Ok(())
}

#[cfg(test)]
mod service_log_tests {
    use super::*;

    #[test]
    fn repeated_pipe_errors_are_bounded_and_report_the_suppressed_count() {
        let start = Instant::now();
        let mut gate = ErrorLogGate::default();
        assert_eq!(gate.admit(start), Some(0));
        assert_eq!(gate.admit(start + Duration::from_secs(1)), None);
        assert_eq!(gate.admit(start + Duration::from_secs(2)), None);
        assert_eq!(gate.admit(start + SESSION_ERROR_LOG_INTERVAL), Some(2));
    }
}
