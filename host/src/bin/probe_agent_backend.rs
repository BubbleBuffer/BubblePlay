#![allow(dead_code)]

use bubbleplay_host::agent_runtime;
use bubbleplay_host::desktop_session;
use bubbleplay_host::privileged_backend;
use bubbleplay_host::privileged_ipc;

use anyhow::{Context, Result};

use agent_runtime::AgentBackend;
use desktop_session::DesktopKind;
use privileged_backend::RealAgentBackend;
use privileged_ipc::Message;

fn main() -> Result<()> {
    let mut backend = RealAgentBackend::new();
    let binding = backend
        .transition_desktop(DesktopKind::Default, 1)
        .context("bind capture and input workers to the default desktop")?;
    let monitors = backend
        .list_monitors()
        .context("enumerate monitors through the independent input worker")?;
    let monitor_count = match monitors {
        Some(Message::MonitorList(monitors)) => monitors.len(),
        Some(other) => anyhow::bail!("unexpected monitor response: {:?}", other.kind()),
        None => anyhow::bail!("monitor worker returned no response"),
    };
    println!(
        "backend probe ok: work_ready={} input_ready={} monitors={monitor_count}",
        binding.work_ready, binding.input_ready
    );
    Ok(())
}
