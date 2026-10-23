//! Deployment settings from the Worker's vars and secrets. The rules are the native server's
//! (server/src/config.rs). Error messages name a field and never print its value.

use bp_device_key::relay_origin;
use bp_relay_core::gate::{GateConfig, MAX_APPROVAL_TTL_SECS, MIN_APPROVAL_TTL_SECS, NtfyConfig};
use bp_relay_core::relay::{CoreConfig, TurnConfig};
use worker::url::Host;
use worker::{Env, Url};

/// What the Durable Object needs: the relay's config and the gate's config.
pub struct Settings {
    pub core: CoreConfig,
    pub gate: GateConfig,
}

const DEFAULT_MAX_CONNECTIONS: usize = 32;
const DEFAULT_APPROVAL_TTL_S: u64 = 90;
const DEFAULT_TURN_TTL_S: u64 = 21_600;
const TURN_TTL_RANGE: std::ops::RangeInclusive<u64> = 300..=86_400;

/// Reads the whole configuration. Every rule is checked here, before the relay starts.
pub fn load(env: &Env) -> Result<Settings, String> {
    let public_origin = canonical_public_origin(&required_var(env, "PUBLIC_ORIGIN")?)?;
    let token = required_secret(env, "RELAY_TOKEN")?;
    // ADMIN_TOKEN is checked like the native one and never stored: M3-C2 reads it.
    required_secret(env, "ADMIN_TOKEN")?;

    let max_connections = match optional_var(env, "MAX_CONNECTIONS") {
        None => DEFAULT_MAX_CONNECTIONS,
        Some(text) => match text.parse::<usize>() {
            Ok(value) if value >= 1 => value,
            _ => return Err("MAX_CONNECTIONS must be a whole number of at least 1".to_owned()),
        },
    };
    let approval_ttl_secs = match optional_var(env, "APPROVAL_TTL_S") {
        None => DEFAULT_APPROVAL_TTL_S,
        Some(text) => text
            .parse::<u64>()
            .ok()
            .filter(|value| (MIN_APPROVAL_TTL_SECS..=MAX_APPROVAL_TTL_SECS).contains(value))
            .ok_or_else(|| "APPROVAL_TTL_S must be 30..120 seconds".to_owned())?,
    };

    let turn = if optional_var(env, "TURN_HOST").is_some()
        || optional_secret(env, "TURN_SECRET").is_some()
    {
        Some(turn_config(env)?)
    } else {
        None
    };
    let ntfy = if optional_var(env, "NTFY_URL").is_some()
        || optional_var(env, "NTFY_TOPIC").is_some()
        || optional_secret(env, "NTFY_TOKEN").is_some()
    {
        Some(ntfy_config(env)?)
    } else {
        None
    };

    let mut core = CoreConfig::new(public_origin.clone(), token);
    core.max_connections = max_connections;
    core.turn = turn;
    let gate = GateConfig {
        public_origin,
        approval_ttl_secs,
        ntfy,
    };
    Ok(Settings { core, gate })
}

/// The canonical origin from the device-key helper. Plain http is allowed only for loopback
/// hosts, as in the native server.
fn canonical_public_origin(input: &str) -> Result<String, String> {
    let origin = relay_origin(input).ok_or_else(|| {
        "PUBLIC_ORIGIN must be a URL with an http, https, ws or wss scheme and a host, without credentials"
            .to_owned()
    })?;
    let url = Url::parse(&origin).map_err(|_| "PUBLIC_ORIGIN is not a valid URL".to_owned())?;
    if url.scheme() == "https" {
        return Ok(origin);
    }
    let loopback = match url.host() {
        Some(Host::Domain(name)) => name == "localhost",
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    };
    if loopback {
        Ok(origin)
    } else {
        Err(
            "PUBLIC_ORIGIN must be an https origin; plain http is allowed only for loopback hosts"
                .to_owned(),
        )
    }
}

fn turn_config(env: &Env) -> Result<TurnConfig, String> {
    let host = optional_var(env, "TURN_HOST")
        .ok_or_else(|| "TURN_HOST is required when TURN is set".to_owned())?;
    if host.trim().is_empty() {
        return Err("TURN_HOST must not be empty".to_owned());
    }
    let secret = optional_secret(env, "TURN_SECRET")
        .ok_or_else(|| "TURN_SECRET is required when TURN is set".to_owned())?;
    if secret.len() < 32 {
        return Err("TURN_SECRET must be at least 32 bytes".to_owned());
    }
    let ttl_s = match optional_var(env, "TURN_TTL_S") {
        None => DEFAULT_TURN_TTL_S,
        Some(text) => text
            .parse::<u64>()
            .ok()
            .filter(|value| TURN_TTL_RANGE.contains(value))
            .ok_or_else(|| "TURN_TTL_S must be 300..86400 seconds".to_owned())?,
    };
    Ok(TurnConfig {
        host,
        secret,
        ttl_s,
    })
}

fn ntfy_config(env: &Env) -> Result<NtfyConfig, String> {
    let url = optional_var(env, "NTFY_URL")
        .ok_or_else(|| "NTFY_URL is required when ntfy is set".to_owned())?;
    let parsed = Url::parse(&url).map_err(|_| "NTFY_URL is not a valid URL".to_owned())?;
    if parsed.scheme() != "https" && parsed.scheme() != "http" {
        return Err("NTFY_URL must be an http or https URL".to_owned());
    }
    let topic = optional_var(env, "NTFY_TOPIC")
        .ok_or_else(|| "NTFY_TOPIC is required when ntfy is set".to_owned())?;
    if topic.trim().is_empty() {
        return Err("NTFY_TOPIC must not be empty".to_owned());
    }
    let token = match optional_secret(env, "NTFY_TOKEN") {
        None => None,
        Some(value) if value.is_empty() => return Err("NTFY_TOKEN is empty".to_owned()),
        Some(value) => Some(value),
    };
    Ok(NtfyConfig { url, topic, token })
}

fn optional_var(env: &Env, name: &str) -> Option<String> {
    env.var(name).ok().map(|value| value.to_string())
}

fn optional_secret(env: &Env, name: &str) -> Option<String> {
    env.secret(name).ok().map(|value| value.to_string())
}

fn required_var(env: &Env, name: &str) -> Result<String, String> {
    optional_var(env, name).ok_or_else(|| format!("{name} is not set"))
}

fn required_secret(env: &Env, name: &str) -> Result<String, String> {
    let value = optional_secret(env, name).ok_or_else(|| format!("{name} is not set"))?;
    if value.is_empty() {
        return Err(format!("{name} is empty"));
    }
    Ok(value)
}
