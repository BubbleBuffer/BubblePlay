#[path = "../desktop_session.rs"]
mod desktop_session;
#[path = "../input.rs"]
mod input;
#[path = "../input_proxy_protocol.rs"]
mod input_proxy_protocol;
#[path = "../input_proxy_runtime.rs"]
mod input_proxy_runtime;

use anyhow::{Context, Result};

fn main() -> Result<()> {
    let mut session_id = None;
    let mut generation = None;
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--session" => {
                session_id = Some(
                    args.next()
                        .context("--session requires a value")?
                        .parse::<u32>()
                        .context("--session must be an unsigned integer")?,
                );
            }
            "--generation" => {
                generation = Some(
                    args.next()
                        .context("--generation requires a value")?
                        .parse::<u32>()
                        .context("--generation must be an unsigned integer")?,
                );
            }
            _ => anyhow::bail!("unknown input proxy argument"),
        }
    }
    input_proxy_runtime::run(
        session_id.context("missing --session")?,
        generation.context("missing --generation")?,
    )
}
