#![allow(dead_code)]

#[path = "../agent_runtime.rs"]
mod agent_runtime;
#[path = "../capture.rs"]
mod capture;
#[path = "../desktop_session.rs"]
mod desktop_session;
#[path = "../encode.rs"]
mod encode;
#[path = "../frame_damage.rs"]
mod frame_damage;
#[path = "../input.rs"]
mod input;
#[path = "../input_proxy_client.rs"]
mod input_proxy_client;
#[path = "../input_proxy_protocol.rs"]
mod input_proxy_protocol;
#[path = "../privileged_backend.rs"]
mod privileged_backend;
#[path = "../privileged_ipc.rs"]
mod privileged_ipc;
#[path = "../privileged_pipe.rs"]
mod privileged_pipe;
#[path = "../quality.rs"]
mod quality;
#[path = "../scale.rs"]
mod scale;

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
