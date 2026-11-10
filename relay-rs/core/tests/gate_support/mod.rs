//! Shared helpers for the gate tests: a deterministic RNG, a config, request
//! builders and small parsers for the responses and ntfy effects.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::net::IpAddr;

use bp_relay_core::RelayRng;
use bp_relay_core::gate::{
    DEFAULT_AUTH_FAILURE_RATE, DEFAULT_NTFY_RATE, GATE_HOLD_MS, Gate, GateConfig, GateEffect,
    GateSnapshot, Hold, HoldCheck, NtfyConfig, Request, Response, Verdict,
};

pub const ORIGIN: &str = "https://relay.example";
pub const T0: u64 = 1_000_000;
pub const TTL_MS: u64 = 90_000;
pub const HOLD_MS: u64 = GATE_HOLD_MS;
/// The client that posts decisions in the helpers. Its prefix is not CLIENT's.
pub const DECIDER: &str = "198.51.100.200";
pub const CLIENT: &str = "203.0.113.7";
pub const OTHER_PREFIX: &str = "198.51.100.7";

/// Splitmix64 bytes: the same seed always gives the same stream.
pub struct TestRng(u64, BTreeMap<IpAddr, String>);

impl TestRng {
    pub fn new(seed: u64) -> Self {
        TestRng(seed, BTreeMap::new())
    }

    /// Simulate the same browser moving to another address.
    pub fn copy_wait_cookie(&mut self, from: &str, to: &str) {
        let cookie = self.1.get(&ip(from)).expect("waiting cookie").clone();
        self.1.insert(ip(to), cookie);
    }

    pub fn wait_cookie(&self, client: &str) -> &str {
        self.1.get(&ip(client)).expect("waiting cookie")
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
        ntfy_rate: DEFAULT_NTFY_RATE,
        auth_failure_rate: DEFAULT_AUTH_FAILURE_RATE,
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
        query: None,
        websocket: false,
        origin: None,
        cookie: None,
        authorization: None,
        sec_fetch_mode: Some("navigate"),
        sec_fetch_dest: Some("document"),
        body: &[],
        client_ip: ip(client),
        now_ms,
    }
}

pub fn get(gate: &mut Gate, rng: &mut TestRng, client: &str, now: u64) -> Verdict {
    navigate_request(gate, rng, &req("GET", "/", client, now))
}

/// Simulate a browser retaining the waiting cookie and following its initial
/// same-URL redirect. Effects from the redirect accompany the resulting hold.
pub fn navigate_request(gate: &mut Gate, rng: &mut TestRng, req: &Request<'_>) -> Verdict {
    let cookie = rng.1.get(&req.client_ip).cloned();
    let mut request = *req;
    if request.cookie.is_none() {
        request.cookie = cookie.as_deref();
    }
    let verdict = gate.handle(rng, &request);
    let Verdict::Handled(response, mut effects) = verdict else {
        return verdict;
    };
    let wait = header(&response, "Set-Cookie")
        .and_then(|value| value.split(';').next())
        .filter(|value| value.starts_with("__Host-bp_wait="))
        .map(str::to_string);
    let Some(wait) = wait else {
        return Verdict::Handled(response, effects);
    };
    assert_eq!(response.status, 302);
    assert!(response.body.is_empty());
    rng.1.insert(req.client_ip, wait.clone());
    let follow = Request {
        cookie: Some(&wait),
        ..*req
    };
    match gate.handle(rng, &follow) {
        Verdict::Hold(hold, more) => {
            effects.extend(more);
            Verdict::Hold(hold, effects)
        }
        other => panic!("waiting redirect must enter hold, got {other:?}"),
    }
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
    navigate_request(gate, rng, &request)
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
        query: None,
        websocket: false,
        origin: None,
        cookie: None,
        authorization: Some(&authorization),
        sec_fetch_mode: None,
        sec_fetch_dest: None,
        body: action.as_bytes(),
        client_ip: ip(client),
        now_ms: now,
    };
    gate.handle(rng, &request)
}

/// The hold and effects of a held navigation. Panics on any other verdict.
pub fn hold_parts(verdict: Verdict) -> (Hold, Vec<GateEffect>) {
    match verdict {
        Verdict::Hold(hold, effects) => (hold, effects),
        other => panic!("expected a hold, got {other:?}"),
    }
}

/// Ask the gate about a held navigation at `now`.
pub fn check(gate: &mut Gate, rng: &mut TestRng, hold: &Hold, now: u64) -> HoldCheck {
    gate.check_hold(rng, hold, now)
}

/// The 302 and effects of an admitted hold. Panics on Waiting or Drop.
pub fn admit_parts(check: HoldCheck) -> (Response, Vec<GateEffect>) {
    match check {
        HoldCheck::Admit(response, effects) => (response, effects),
        other => panic!("expected an admit, got {other:?}"),
    }
}

/// Open a held navigation for `client` at `now` and return the hold and the
/// approve and deny bearers its ntfy publish carries.
pub fn open_hold(
    gate: &mut Gate,
    rng: &mut TestRng,
    client: &str,
    now: u64,
) -> (Hold, String, String) {
    let (hold, effects) = hold_parts(navigate_request(gate, rng, &req("GET", "/", client, now)));
    assert_eq!(effects.len(), 1, "a new approval publishes once");
    let (approve, deny) = bearer_tokens(&effects[0]);
    (hold, approve, deny)
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

/// The `__Host-gate=<value>` pair from a Set-Cookie header, ready to send as a
/// Cookie header. None when the response sets no gate cookie.
pub fn set_cookie_value(response: &Response) -> Option<String> {
    let value = header(response, "Set-Cookie")?;
    let pair = value.split(';').next()?;
    pair.starts_with("__Host-gate=").then(|| pair.to_string())
}

/// Run the whole approval for `client` and return the cookie it earns: the
/// navigation is held, approved, and admitted at `now + 2_000`.
pub fn approve_client(gate: &mut Gate, rng: &mut TestRng, client: &str, now: u64) -> String {
    let (hold, approve, _) = open_hold(gate, rng, client, now);
    decide(gate, rng, &approve, "approve", DECIDER, now + 1_000);
    let (response, _) = admit_parts(check(gate, rng, &hold, now + 2_000));
    set_cookie_value(&response).expect("cookie set on approval")
}

/// Whether a verdict drops the request.
pub fn is_drop(verdict: &Verdict) -> bool {
    matches!(verdict, Verdict::Drop)
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
