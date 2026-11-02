//! Wires the core's structured log lines (`bp_relay_core::log`) to stderr, next to the
//! tracing output. One JSON object per line, spec section 1 of the logging lane.

use std::io::Write;

use bp_relay_core::log::{self, Level};

use crate::config::Config;

/// Installs the stderr sink and registers the configured secrets, so no log line can
/// carry them even by mistake. Call once, before the relay starts.
pub fn install(cfg: &Config) {
    log::set_sink(write_line);
    log::register_secret(cfg.token.expose());
    log::register_secret(cfg.admin_token.expose());
    if let Some(turn) = &cfg.turn {
        log::register_secret(turn.secret.expose());
    }
    if let Some(token) = cfg.ntfy.as_ref().and_then(|ntfy| ntfy.token.as_ref()) {
        log::register_secret(token.expose());
    }
}

fn write_line(_level: Level, line: &str) {
    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(stderr, "{line}");
}
