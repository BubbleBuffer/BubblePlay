//! Shared helpers for the gate tests: a deterministic RNG, a config, request
//! builders and small parsers for the responses and ntfy effects.
#![allow(dead_code)]

use std::net::IpAddr;

use bp_relay_core::RelayRng;
use bp_relay_core::gate::{
    Gate, GateConfig, GateEffect, GateSnapshot, NtfyConfig, Request, Response, Verdict,
};

pub const ORIGIN: &str = "https://relay.example";
pub const T0: u64 = 1_000_000;
pub const TTL_MS: u64 = 90_000;
pub const CLIENT: &str = "203.0.113.7";
pub const OTHER_PREFIX: &str = "198.51.100.7";

/// Splitmix64 bytes: the same seed always gives the same stream.
pub struct TestRng(u64);

impl TestRng {
    pub fn new(seed: u64) -> Self {
        TestRng(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

impl RelayRng for TestRng {
    fn fill(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let bytes = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
    }
}

pub fn config(ntfy: bool) -> GateConfig {
    GateConfig {
        public_origin: ORIGIN.to_string(),
        approval_ttl_secs: 90,
        ntfy: ntfy.then(|| NtfyConfig {
            url: "https://ntfy.example".into(),
            topic: "bp-gate".into(),
            token: Some("ntfy-secret".into()),
        }),
    }
}

pub fn new_gate() -> Gate {
    Gate::new(config(true), GateSnapshot::default(), T0).expect("valid config")
}

pub fn ip(text: &str) -> IpAddr {
    text.parse().expect("valid ip")
}

pub fn req<'a>(method: &'a str, path: &'a str, client: &str, now_ms: u64) -> Request<'a> {
    Request {
        method,
        path,
        websocket: false,
        origin: None,
        cookie: None,
        authorization: None,
        sec_fetch_dest: None,
        body: &[],
        client_ip: ip(client),
        now_ms,
    }
}

pub fn get(gate: &mut Gate, rng: &mut TestRng, client: &str, now: u64) -> Verdict {
    gate.handle(rng, &req("GET", "/", client, now))
}

pub fn get_with_cookie(
    gate: &mut Gate,
    rng: &mut TestRng,
    path: &str,
    cookie: &str,
    now: u64,
) -> Verdict {
    let mut request = req("GET", path, CLIENT, now);
    request.cookie = Some(cookie);
    gate.handle(rng, &request)
}

/// POST a decision. `action` is the body, `bearer` the raw token.
pub fn decide(
    gate: &mut Gate,
    rng: &mut TestRng,
    bearer: &str,
    action: &str,
    client: &str,
    now: u64,
) -> Verdict {
    let authorization = format!("Bearer {bearer}");
    let request = Request {
        method: "POST",
        path: "/__gate/decision",
        websocket: false,
        origin: None,
        cookie: None,
        authorization: Some(&authorization),
        sec_fetch_dest: None,
        body: action.as_bytes(),
        client_ip: ip(client),
        now_ms: now,
    };
    gate.handle(rng, &request)
}

pub fn poll(gate: &mut Gate, rng: &mut TestRng, token: &str, client: &str, now: u64) -> Verdict {
    let body = format!("{{\"pending\":\"{token}\"}}");
    let request = Request {
        method: "POST",
        path: "/__gate/poll",
        websocket: false,
        origin: Some(ORIGIN),
        cookie: None,
        authorization: None,
        sec_fetch_dest: None,
        body: body.as_bytes(),
        client_ip: ip(client),
        now_ms: now,
    };
    gate.handle(rng, &request)
}

pub fn wait_parts(verdict: Verdict) -> (Response, Vec<GateEffect>) {
    match verdict {
        Verdict::Wait(response, effects) => (response, effects),
        other => panic!("expected a wait page, got {other:?}"),
    }
}

pub fn handled_parts(verdict: Verdict) -> (Response, Vec<GateEffect>) {
    match verdict {
        Verdict::Handled(response, effects) => (response, effects),
        other => panic!("expected a handled response, got {other:?}"),
    }
}

pub fn header<'a>(response: &'a Response, name: &str) -> Option<&'a str> {
    response
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// The poll token the wait page carries.
pub fn poll_token(response: &Response) -> String {
    let html = String::from_utf8(response.body.clone()).expect("utf-8 page");
    let marker = "var token = \"";
    let start = html.find(marker).expect("token in page") + marker.len();
    let end = start + html[start..].find('"').expect("token end");
    html[start..end].to_string()
}

/// The parsed JSON body of an ntfy effect.
pub fn ntfy_json(effect: &GateEffect) -> serde_json::Value {
    match effect {
        GateEffect::NtfyPublish { body, .. } => serde_json::from_slice(body).expect("json body"),
        other => panic!("expected an ntfy publish, got {other:?}"),
    }
}

/// The approve and deny bearer tokens from an ntfy effect.
pub fn bearer_tokens(effect: &GateEffect) -> (String, String) {
    let json = ntfy_json(effect);
    let actions = json["actions"].as_array().expect("actions");
    let token = |index: usize| {
        actions[index]["headers"]["Authorization"]
            .as_str()
            .expect("authorization")
            .trim_start_matches("Bearer ")
            .to_string()
    };
    (token(0), token(1))
}

pub fn state_of(response: &Response) -> String {
    let value: serde_json::Value = serde_json::from_slice(&response.body).expect("json");
    value["state"].as_str().expect("state").to_string()
}

/// The `__Host-gate=<value>` pair from a Set-Cookie header, ready to send as a
/// Cookie header. None when the response sets no gate cookie.
pub fn set_cookie_value(response: &Response) -> Option<String> {
    let value = header(response, "Set-Cookie")?;
    let pair = value.split(';').next()?;
    pair.starts_with("__Host-gate=").then(|| pair.to_string())
}

/// Run the whole approval for `client` and return the cookie it earns.
pub fn approve_client(gate: &mut Gate, rng: &mut TestRng, client: &str, now: u64) -> String {
    let (page, effects) = wait_parts(gate.handle(rng, &req("GET", "/", client, now)));
    let token = poll_token(&page);
    let (approve, _) = bearer_tokens(&effects[0]);
    decide(
        gate,
        rng,
        &approve,
        "approve",
        "198.51.100.200",
        now + 1_000,
    );
    let (response, _) = handled_parts(poll(gate, rng, &token, client, now + 2_000));
    set_cookie_value(&response).expect("cookie set on approval")
}

pub fn snapshot_of(effects: &[GateEffect]) -> GateSnapshot {
    effects
        .iter()
        .find_map(|effect| match effect {
            GateEffect::PersistGate(snapshot) => Some(snapshot.clone()),
            _ => None,
        })
        .expect("a PersistGate effect")
}
