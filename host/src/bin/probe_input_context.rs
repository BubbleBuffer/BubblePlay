#[path = "../desktop_session.rs"]
mod desktop_session;

use anyhow::Result;
use desktop_session::InputDesktop;

fn main() -> Result<()> {
    let bound = InputDesktop::open()?.bind_thread()?;
    let context = bound.security_context()?;
    println!("{context:#?}");
    Ok(())
}
