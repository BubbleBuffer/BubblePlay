//! `relay.json`: loading, validation and the environment overrides for the two bearer secrets.
//!
//! Every validation error names the field it is about. Secrets are wrapped in [`Secret`], whose
//! `Debug` output is redacted, so a `Config` can be logged without leaking tokens.

use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use bp_device_key::relay_origin;
use bp_relay_core::gate::GATE_HOLD_MS;
use ipnet::IpNet;
use serde::Deserialize;
use url::Url;

pub const DEFAULT_LISTEN: &str = "0.0.0.0:8080";
pub const DEFAULT_ASSETS_DIR: &str = "/assets";
pub const DEFAULT_STATE_DIR: &str = "/data";
pub const DEFAULT_MAX_CONNECTIONS: u32 = 32;
pub const DEFAULT_APPROVAL_TTL_S: u32 = 90;
pub const DEFAULT_TURN_TTL_S: u32 = 21_600;
pub const ENV_TOKEN: &str = "BP_RELAY_TOKEN";
pub const ENV_ADMIN_TOKEN: &str = "BP_RELAY_ADMIN_TOKEN";

/// A bearer secret. `Debug` never prints the value.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// TURN credential issuance. Optional in `relay.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    pub host: String,
    pub secret: Secret,
    pub ttl_s: u32,
}

/// ntfy push settings for approval requests. Optional in `relay.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ntfy {
    pub url: String,
    pub topic: String,
    pub token: Option<Secret>,
}

/// The validated relay configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub public_origin: String,
    pub listen: SocketAddr,
    pub assets_dir: PathBuf,
    pub state_dir: PathBuf,
    pub token: Secret,
    pub admin_token: Secret,
    pub max_connections: u32,
    pub max_unauthenticated_connections: u32,
    pub max_unauthenticated_per_prefix: u32,
    pub upgrade_rate_per_prefix: u32,
    pub ntfy_publish_limit: u32,
    pub ntfy_publish_window_s: u32,
    pub auth_failure_limit: u32,
    pub auth_failure_window_s: u32,

    pub turn: Option<Turn>,
    pub ntfy: Option<Ntfy>,
    pub approval_ttl_s: u32,
    pub trusted_proxy_cidrs: Vec<IpNet>,
    /// How long the gate holds an unapproved navigation. Not read from relay.json: it is always
    /// `GATE_HOLD_MS` there, and tests shorten it. The gate clamps it to `1..=GATE_HOLD_MS`.
    pub gate_hold_ms: u64,
}

#[derive(Debug)]
pub enum ConfigError {
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    Parse(String),
    Invalid {
        field: String,
        reason: String,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => write!(f, "cannot read {}: {source}", path.display()),
            Self::Parse(message) => write!(f, "relay.json: {message}"),
            Self::Invalid { field, reason } => write!(f, "{field}: {reason}"),
        }
    }
}

impl std::error::Error for ConfigError {}

fn positive(field: &str, value: Option<u32>, default: u32) -> Result<u32, ConfigError> {
    value
        .filter(|v| *v > 0)
        .or_else(|| value.is_none().then_some(default))
        .ok_or_else(|| invalid(field, "must be at least 1"))
}

fn invalid(field: &str, reason: impl Into<String>) -> ConfigError {
    ConfigError::Invalid {
        field: field.to_string(),
        reason: reason.into(),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    public_origin: Option<String>,
    listen: Option<String>,
    assets_dir: Option<String>,
    state_dir: Option<String>,
    token: Option<String>,
    admin_token: Option<String>,
    max_connections: Option<u32>,
    max_unauthenticated_connections: Option<u32>,
    max_unauthenticated_per_prefix: Option<u32>,
    upgrade_rate_per_prefix: Option<u32>,
    ntfy_publish_limit: Option<u32>,
    ntfy_publish_window_s: Option<u32>,
    auth_failure_limit: Option<u32>,
    auth_failure_window_s: Option<u32>,

    turn: Option<RawTurn>,
    ntfy: Option<RawNtfy>,
    approval_ttl_s: Option<u32>,
    trusted_proxy_cidrs: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTurn {
    host: Option<String>,
    secret: Option<String>,
    ttl_s: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawNtfy {
    url: Option<String>,
    topic: Option<String>,
    token: Option<String>,
}

/// Reads `path` and validates it. The two secrets may be overridden by `BP_RELAY_TOKEN` and
/// `BP_RELAY_ADMIN_TOKEN` in the process environment.
pub fn load_config(path: &Path) -> Result<Config, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    parse_config(&text, &|name: &str| std::env::var(name).ok())
}

/// Parses and validates relay.json text. `env` looks up environment variables, so tests can
/// supply their own.
pub fn parse_config(
    text: &str,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Config, ConfigError> {
    let raw: RawConfig =
        serde_json::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))?;

    let public_origin = canonical_public_origin(
        raw.public_origin
            .as_deref()
            .ok_or_else(|| invalid("public_origin", "is required"))?,
    )?;

    let listen_text = raw.listen.unwrap_or_else(|| DEFAULT_LISTEN.to_string());
    let listen: SocketAddr = listen_text.parse().map_err(|_| {
        invalid(
            "listen",
            "must be an IP address and a port, such as 0.0.0.0:8080",
        )
    })?;

    let assets_dir = non_empty_path("assets_dir", raw.assets_dir, DEFAULT_ASSETS_DIR)?;
    let state_dir = non_empty_path("state_dir", raw.state_dir, DEFAULT_STATE_DIR)?;

    let token = secret_field("token", ENV_TOKEN, raw.token, env)?;
    let admin_token = secret_field("admin_token", ENV_ADMIN_TOKEN, raw.admin_token, env)?;

    let max_connections = raw.max_connections.unwrap_or(DEFAULT_MAX_CONNECTIONS);
    if max_connections == 0 {
        return Err(invalid("max_connections", "must be at least 1"));
    }

    let max_unauthenticated_connections = positive(
        "max_unauthenticated_connections",
        raw.max_unauthenticated_connections,
        16,
    )?;
    let max_unauthenticated_per_prefix = positive(
        "max_unauthenticated_per_prefix",
        raw.max_unauthenticated_per_prefix,
        4,
    )?;
    let upgrade_rate_per_prefix =
        positive("upgrade_rate_per_prefix", raw.upgrade_rate_per_prefix, 30)?;
    let ntfy_publish_limit = positive("ntfy_publish_limit", raw.ntfy_publish_limit, 10)?;
    let ntfy_publish_window_s = positive("ntfy_publish_window_s", raw.ntfy_publish_window_s, 600)?;
    let auth_failure_limit = positive("auth_failure_limit", raw.auth_failure_limit, 5)?;
    let auth_failure_window_s = positive("auth_failure_window_s", raw.auth_failure_window_s, 600)?;

    let approval_ttl_s = raw.approval_ttl_s.unwrap_or(DEFAULT_APPROVAL_TTL_S);
    if !(30..=120).contains(&approval_ttl_s) {
        return Err(invalid("approval_ttl_s", "must be 30..120 seconds"));
    }

    let mut trusted_proxy_cidrs = Vec::new();
    for entry in raw.trusted_proxy_cidrs.unwrap_or_default() {
        let network: IpNet = entry.parse().map_err(|_| {
            invalid(
                "trusted_proxy_cidrs",
                format!("'{entry}' is not a CIDR range"),
            )
        })?;
        trusted_proxy_cidrs.push(network);
    }

    let turn = raw.turn.map(parse_turn).transpose()?;
    let ntfy = raw.ntfy.map(parse_ntfy).transpose()?;

    Ok(Config {
        public_origin,
        listen,
        assets_dir,
        state_dir,
        token,
        admin_token,
        max_connections,
        max_unauthenticated_connections,
        max_unauthenticated_per_prefix,
        upgrade_rate_per_prefix,
        ntfy_publish_limit,
        ntfy_publish_window_s,
        auth_failure_limit,
        auth_failure_window_s,

        turn,
        ntfy,
        approval_ttl_s,
        trusted_proxy_cidrs,
        gate_hold_ms: GATE_HOLD_MS,
    })
}

/// Canonical origin via the shared device-key helper. Plain http is allowed only for loopback
/// hosts, as in the Go relay.
fn canonical_public_origin(input: &str) -> Result<String, ConfigError> {
    let origin = relay_origin(input).ok_or_else(|| {
        invalid(
            "public_origin",
            "must be a URL with an http, https, ws or wss scheme and a host, without credentials",
        )
    })?;
    let url = Url::parse(&origin).map_err(|_| invalid("public_origin", "is not a valid URL"))?;
    if url.scheme() == "https" {
        return Ok(origin);
    }
    let loopback = match url.host() {
        Some(url::Host::Domain(name)) => name == "localhost",
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    };
    if loopback {
        Ok(origin)
    } else {
        Err(invalid(
            "public_origin",
            "must be an https origin; plain http is allowed only for loopback hosts",
        ))
    }
}

fn non_empty_path(
    field: &str,
    value: Option<String>,
    default: &str,
) -> Result<PathBuf, ConfigError> {
    let value = value.unwrap_or_else(|| default.to_string());
    if value.trim().is_empty() {
        return Err(invalid(field, "must not be empty"));
    }
    Ok(PathBuf::from(value))
}

/// The environment variable wins over the file. Neither source may be empty.
fn secret_field(
    field: &str,
    env_name: &str,
    file_value: Option<String>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Secret, ConfigError> {
    if let Some(value) = env(env_name) {
        if value.is_empty() {
            return Err(invalid(env_name, "is set but empty"));
        }
        return strong_secret(env_name, value);
    }
    match file_value {
        None => Err(invalid(
            field,
            format!("is required; set it in relay.json or in {env_name}"),
        )),
        Some(value) if value.is_empty() => Err(invalid(field, "is empty")),
        Some(value) => strong_secret(field, value),
    }
}

fn strong_secret(field: &str, value: String) -> Result<Secret, ConfigError> {
    if value.chars().count() < 32 {
        return Err(invalid(
            field,
            "must be at least 32 characters; generate fresh 256-bit secrets with bp-relay init <path>",
        ));
    }
    Ok(Secret::new(value))
}

fn parse_turn(raw: RawTurn) -> Result<Turn, ConfigError> {
    let host = raw
        .host
        .ok_or_else(|| invalid("turn.host", "is required when turn is set"))?;
    if host.trim().is_empty() {
        return Err(invalid("turn.host", "must not be empty"));
    }
    let secret = raw
        .secret
        .ok_or_else(|| invalid("turn.secret", "is required when turn is set"))?;
    if secret.len() < 32 {
        return Err(invalid("turn.secret", "must be at least 32 bytes"));
    }
    let ttl_s = raw.ttl_s.unwrap_or(DEFAULT_TURN_TTL_S);
    if !(300..=86_400).contains(&ttl_s) {
        return Err(invalid("turn.ttl_s", "must be 300..86400 seconds"));
    }
    Ok(Turn {
        host,
        secret: Secret::new(secret),
        ttl_s,
    })
}

fn parse_ntfy(raw: RawNtfy) -> Result<Ntfy, ConfigError> {
    let url = raw
        .url
        .ok_or_else(|| invalid("ntfy.url", "is required when ntfy is set"))?;
    let parsed = Url::parse(&url).map_err(|_| invalid("ntfy.url", "is not a valid URL"))?;
    if parsed.scheme() != "https" && parsed.scheme() != "http" {
        return Err(invalid("ntfy.url", "must be an http or https URL"));
    }
    let topic = raw
        .topic
        .ok_or_else(|| invalid("ntfy.topic", "is required when ntfy is set"))?;
    if topic.trim().is_empty() {
        return Err(invalid("ntfy.topic", "must not be empty"));
    }
    let token = match raw.token {
        None => None,
        Some(value) if value.is_empty() => return Err(invalid("ntfy.token", "is empty")),
        Some(value) => Some(Secret::new(value)),
    };
    Ok(Ntfy { url, topic, token })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> String {
        r#"{"public_origin":"https://relay.example.com","token":"tok-onexxxxxxxxxxxxxxxxxxxxxxxxx","admin_token":"adm-onexxxxxxxxxxxxxxxxxxxxxxxxx"}"#
            .to_string()
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn parse(text: &str) -> Result<Config, ConfigError> {
        parse_config(text, &no_env)
    }

    fn error_text(result: Result<Config, ConfigError>) -> String {
        match result {
            Ok(_) => panic!("expected a validation error"),
            Err(error) => error.to_string(),
        }
    }

    #[test]
    fn short_secrets_are_refused_in_file_and_environment() {
        for field in ["token", "admin_token"] {
            let mut value: serde_json::Value = serde_json::from_str(&base()).unwrap();
            value[field] = "x".repeat(31).into();
            let message = error_text(parse(&value.to_string()));
            assert!(
                message.starts_with(&format!("{field}:"))
                    && message.contains("32 characters")
                    && message.contains("init"),
                "{message}"
            );
        }
        for field in [ENV_TOKEN, ENV_ADMIN_TOKEN] {
            let message = error_text(parse_config(&base(), &|name| {
                (name == field).then(|| "short".into())
            }));
            assert!(
                message.starts_with(field) && message.contains("init"),
                "{message}"
            );
        }
    }

    #[test]
    fn defaults_fill_in_missing_fields() {
        let cfg = parse(&base()).expect("minimal config parses");
        assert_eq!(cfg.listen, "0.0.0.0:8080".parse::<SocketAddr>().unwrap());
        assert_eq!(cfg.assets_dir, PathBuf::from("/assets"));
        assert_eq!(cfg.state_dir, PathBuf::from("/data"));
        assert_eq!(cfg.max_connections, 32);
        assert_eq!(cfg.approval_ttl_s, 90);
        assert!(cfg.turn.is_none());
        assert!(cfg.ntfy.is_none());
        assert!(cfg.trusted_proxy_cidrs.is_empty());
    }

    #[test]
    fn public_origin_is_canonicalised() {
        let text = base().replace(
            "https://relay.example.com",
            "wss://Relay.Example.com:443/some/path",
        );
        let cfg = parse(&text).expect("wss origin is accepted");
        assert_eq!(cfg.public_origin, "https://relay.example.com");
    }

    #[test]
    fn plain_http_origin_needs_loopback() {
        let text = base().replace("https://relay.example.com", "http://relay.example.com");
        assert!(error_text(parse(&text)).starts_with("public_origin:"));
        let local = base().replace("https://relay.example.com", "http://127.0.0.1:8080");
        assert_eq!(
            parse(&local).unwrap().public_origin,
            "http://127.0.0.1:8080"
        );
    }

    #[test]
    fn missing_public_origin_names_the_field() {
        let text = r#"{"token":"a","admin_token":"b"}"#;
        assert!(error_text(parse(text)).starts_with("public_origin:"));
    }

    #[test]
    fn unknown_field_is_an_error() {
        let text = base().replace('{', r#"{"bogus_field":1,"#);
        assert!(error_text(parse(&text)).contains("bogus_field"));
    }

    #[test]
    fn env_overrides_both_secrets() {
        let env = |name: &str| match name {
            ENV_TOKEN => Some("env-tokenxxxxxxxxxxxxxxxxxxxxxxx".to_string()),
            ENV_ADMIN_TOKEN => Some("env-adminxxxxxxxxxxxxxxxxxxxxxxx".to_string()),
            _ => None,
        };
        let cfg = parse_config(&base(), &env).unwrap();
        assert_eq!(cfg.token.expose(), "env-tokenxxxxxxxxxxxxxxxxxxxxxxx");
        assert_eq!(cfg.admin_token.expose(), "env-adminxxxxxxxxxxxxxxxxxxxxxxx");
    }

    #[test]
    fn empty_env_secret_is_refused_by_name() {
        let env = |name: &str| (name == ENV_TOKEN).then(String::new);
        let message = error_text(parse_config(&base(), &env));
        assert!(message.contains(ENV_TOKEN), "{message}");
    }

    #[test]
    fn missing_token_names_the_field() {
        let text = r#"{"public_origin":"https://relay.example.com","admin_token":"adm"}"#;
        assert!(error_text(parse(text)).starts_with("token:"));
    }

    #[test]
    fn debug_output_never_shows_secrets() {
        let cfg = parse(&base()).unwrap();
        let shown = format!("{cfg:?}");
        assert!(
            !shown.contains("tok-onexxxxxxxxxxxxxxxxxxxxxxxxx"),
            "{shown}"
        );
        assert!(
            !shown.contains("adm-onexxxxxxxxxxxxxxxxxxxxxxxxx"),
            "{shown}"
        );
    }

    #[test]
    fn approval_ttl_range_is_enforced() {
        let low = base().replace('{', r#"{"approval_ttl_s":29,"#);
        let high = base().replace('{', r#"{"approval_ttl_s":121,"#);
        assert!(error_text(parse(&low)).starts_with("approval_ttl_s:"));
        assert!(error_text(parse(&high)).starts_with("approval_ttl_s:"));
        let edge = base().replace('{', r#"{"approval_ttl_s":120,"#);
        assert_eq!(parse(&edge).unwrap().approval_ttl_s, 120);
    }

    #[test]
    fn max_connections_must_be_positive() {
        let text = base().replace('{', r#"{"max_connections":0,"#);
        assert!(error_text(parse(&text)).starts_with("max_connections:"));
    }

    #[test]
    fn bad_listen_names_the_field() {
        let text = base().replace('{', r#"{"listen":"not-an-address","#);
        assert!(error_text(parse(&text)).starts_with("listen:"));
    }

    #[test]
    fn bad_trusted_proxy_cidr_names_the_field() {
        let text = base().replace('{', r#"{"trusted_proxy_cidrs":["10.0.0.1"],"#);
        assert!(error_text(parse(&text)).starts_with("trusted_proxy_cidrs:"));
        let good = base().replace('{', r#"{"trusted_proxy_cidrs":["10.0.0.0/8","fd00::/8"],"#);
        assert_eq!(parse(&good).unwrap().trusted_proxy_cidrs.len(), 2);
    }

    #[test]
    fn turn_secret_needs_32_bytes() {
        let short = base().replace(
            '{',
            r#"{"turn":{"host":"turn.example.com","secret":"short"},"#,
        );
        assert!(error_text(parse(&short)).starts_with("turn.secret:"));
        let long = "x".repeat(32);
        let ok = base().replace(
            '{',
            &format!(r#"{{"turn":{{"host":"turn.example.com","secret":"{long}"}},"#),
        );
        let cfg = parse(&ok).unwrap();
        let turn = cfg.turn.expect("turn parsed");
        assert_eq!(turn.ttl_s, 21_600);
        assert_eq!(turn.host, "turn.example.com");
    }

    #[test]
    fn turn_ttl_range_is_enforced() {
        let secret = "y".repeat(32);
        let text = base().replace(
            '{',
            &format!(r#"{{"turn":{{"host":"t.example.com","secret":"{secret}","ttl_s":299}},"#),
        );
        assert!(error_text(parse(&text)).starts_with("turn.ttl_s:"));
    }

    #[test]
    fn ntfy_block_parses_and_validates() {
        let text = base().replace(
            '{',
            r#"{"ntfy":{"url":"https://ntfy.example.com","topic":"bp","token":"nt-1"},"#,
        );
        let cfg = parse(&text).unwrap();
        let ntfy = cfg.ntfy.expect("ntfy parsed");
        assert_eq!(ntfy.topic, "bp");
        assert_eq!(ntfy.token.as_ref().map(Secret::expose), Some("nt-1"));
        let bad = base().replace('{', r#"{"ntfy":{"url":"ftp://x","topic":"bp"},"#);
        assert!(error_text(parse(&bad)).starts_with("ntfy.url:"));
    }

    #[test]
    fn empty_assets_dir_is_refused() {
        let text = base().replace('{', r#"{"assets_dir":"  ","#);
        assert!(error_text(parse(&text)).starts_with("assets_dir:"));
    }

    #[test]
    fn load_config_reports_a_missing_file_with_its_path() {
        let missing = Path::new("definitely-missing-relay.json");
        let message = match load_config(missing) {
            Ok(_) => panic!("missing file must fail"),
            Err(error) => error.to_string(),
        };
        assert!(
            message.contains("definitely-missing-relay.json"),
            "{message}"
        );
    }
}
