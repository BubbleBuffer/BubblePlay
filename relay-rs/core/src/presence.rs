//! `GET /presence?ids=A,B,C`: which PCs have a host socket open right now.
//!
//! The route is unauthenticated and side-effect free. It reads host presence
//! only (the same state that backs `online` in the admin listing), never
//! touches the gate's approvals, cookies or ntfy, and writes nothing. The gate
//! lets a GET on [`PRESENCE_PATH`] through before its navigation step (see
//! `Gate::handle`), and the adapter answers it with [`answer`].
//!
//! Requests are counted per client prefix (IPv4 /24, IPv6 /56, as the gate
//! counts them) in a fixed window. Over the limit, or with the prefix map full,
//! the answer is 429 with `Retry-After`. Every answer is JSON with
//! `Cache-Control: no-store`.

use std::collections::BTreeMap;
use std::net::IpAddr;

use bp_device_key::DeviceId;
use serde_json::{Map, Value, json};

use crate::gate::{MAX_TRACKED_PREFIXES, Prefix, Response};
use crate::relay::Relay;

/// The route's path. Only a GET on exactly this path is a presence query.
pub const PRESENCE_PATH: &str = "/presence";
/// The most ids one query may name. More is 400.
pub const MAX_PRESENCE_IDS: usize = 20;
/// Presence queries per client prefix per window.
pub const PRESENCE_LIMIT: u32 = 60;
pub const PRESENCE_WINDOW_MS: u64 = 60_000;

/// Why a query was refused before any lookup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PresenceError {
    /// No `ids` parameter, or an empty one.
    MissingIds,
    /// More than [`MAX_PRESENCE_IDS`] ids.
    TooManyIds,
    /// An entry that is not a device id (an empty entry included).
    MalformedId,
}

impl PresenceError {
    fn message(self) -> &'static str {
        match self {
            PresenceError::MissingIds => "missing ids",
            PresenceError::TooManyIds => "too many ids",
            PresenceError::MalformedId => "malformed id",
        }
    }
}

/// Splits the decoded `ids` value on commas and returns each id in canonical
/// form, in request order (a repeated id appears more than once). Nothing is
/// parsed once the count is over [`MAX_PRESENCE_IDS`].
pub fn parse_ids(ids: Option<&str>) -> Result<Vec<String>, PresenceError> {
    let raw = ids.ok_or(PresenceError::MissingIds)?;
    if raw.is_empty() {
        return Err(PresenceError::MissingIds);
    }
    if raw.split(',').count() > MAX_PRESENCE_IDS {
        return Err(PresenceError::TooManyIds);
    }
    raw.split(',')
        .map(|part| {
            DeviceId::parse(part.trim())
                .map(|id| id.to_string())
                .map_err(|_| PresenceError::MalformedId)
        })
        .collect()
}

/// A fixed-window counter per client prefix. Memory only; a restart forgets it.
#[derive(Debug, Default)]
pub struct PresenceLimiter {
    rates: BTreeMap<Prefix, (u64, u32)>,
}

impl PresenceLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Counts one query from `client` at `now_ms`. `Ok` when it may go on;
    /// `Err(seconds)` when it is over the limit, with the seconds until its
    /// window ends (at least 1). A new prefix arriving while the map is full
    /// (after expired windows are dropped) is refused for a whole window.
    pub fn check(&mut self, client: IpAddr, now_ms: u64) -> Result<(), u64> {
        let prefix = Prefix::of(client);
        if !self.rates.contains_key(&prefix) && self.rates.len() >= MAX_TRACKED_PREFIXES {
            self.rates
                .retain(|_, (start, _)| now_ms.saturating_sub(*start) < PRESENCE_WINDOW_MS);
            if self.rates.len() >= MAX_TRACKED_PREFIXES {
                return Err(PRESENCE_WINDOW_MS.div_ceil(1000));
            }
        }
        let entry = self.rates.entry(prefix).or_insert((now_ms, 0));
        if now_ms.saturating_sub(entry.0) >= PRESENCE_WINDOW_MS {
            *entry = (now_ms, 0);
        }
        entry.1 = entry.1.saturating_add(1);
        if entry.1 <= PRESENCE_LIMIT {
            Ok(())
        } else {
            let ends = entry.0.saturating_add(PRESENCE_WINDOW_MS);
            Err(ends.saturating_sub(now_ms).div_ceil(1000).max(1))
        }
    }

    /// Prefixes currently tracked, for tests.
    pub fn tracked(&self) -> usize {
        self.rates.len()
    }
}

/// Answers one presence query: the rate limit first (every query counts,
/// refused ones too), then the ids, then the lookup. `ids` is the decoded
/// value of the first `ids` query parameter, if any.
pub fn answer(
    limiter: &mut PresenceLimiter,
    relay: &Relay,
    client: IpAddr,
    ids: Option<&str>,
    now_ms: u64,
) -> Response {
    if let Err(retry_after) = limiter.check(client, now_ms) {
        let mut response = json_response(429, &json!({ "error": "rate limited" }));
        response
            .headers
            .push(("Retry-After".into(), retry_after.to_string()));
        return response;
    }
    match parse_ids(ids) {
        Ok(ids) => {
            let mut body = Map::new();
            for id in ids {
                let online = relay.pc_online(&id);
                body.insert(id, Value::Bool(online));
            }
            json_response(200, &Value::Object(body))
        }
        Err(error) => json_response(400, &json!({ "error": error.message() })),
    }
}

fn json_response(status: u16, value: &Value) -> Response {
    Response {
        status,
        headers: vec![
            ("Content-Type".into(), "application/json".into()),
            ("Cache-Control".into(), "no-store".into()),
        ],
        body: serde_json::to_vec(value).expect("serializing a JSON value cannot fail"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay::{CoreConfig, PcTable};
    use std::net::{Ipv4Addr, Ipv6Addr};

    const T0: u64 = 1_000_000;

    fn id(seed: u8) -> String {
        DeviceId::from_public_key(&[seed; 32]).to_string()
    }

    fn v4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    fn relay() -> Relay {
        Relay::new(
            CoreConfig::new("https://relay.example", "token"),
            PcTable::new(),
        )
    }

    fn header<'a>(response: &'a Response, name: &str) -> Option<&'a str> {
        response
            .headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn body(response: &Response) -> Value {
        serde_json::from_slice(&response.body).expect("json body")
    }

    #[test]
    fn parse_accepts_up_to_twenty_canonical_ids() {
        let ids: Vec<String> = (0..20).map(id).collect();
        let joined = ids.join(",");
        assert_eq!(parse_ids(Some(&joined)), Ok(ids));
    }

    #[test]
    fn parse_canonicalises_ids() {
        let canonical = id(3);
        let lower = canonical.to_ascii_lowercase();
        assert_eq!(parse_ids(Some(&lower)), Ok(vec![canonical]));
    }

    #[test]
    fn parse_refuses_twenty_one_ids() {
        let joined = (0..21).map(id).collect::<Vec<_>>().join(",");
        assert_eq!(parse_ids(Some(&joined)), Err(PresenceError::TooManyIds));
        // The count is checked before any entry is parsed.
        let junk = vec!["x"; 21].join(",");
        assert_eq!(parse_ids(Some(&junk)), Err(PresenceError::TooManyIds));
    }

    #[test]
    fn parse_refuses_malformed_and_empty_entries() {
        assert_eq!(parse_ids(Some("nope")), Err(PresenceError::MalformedId));
        let trailing = format!("{},", id(1));
        assert_eq!(parse_ids(Some(&trailing)), Err(PresenceError::MalformedId));
        let mixed = format!("{},ZZZZ-ZZZZ", id(1));
        assert_eq!(parse_ids(Some(&mixed)), Err(PresenceError::MalformedId));
        assert_eq!(parse_ids(None), Err(PresenceError::MissingIds));
        assert_eq!(parse_ids(Some("")), Err(PresenceError::MissingIds));
    }

    #[test]
    fn limiter_allows_the_bound_then_429s_per_prefix() {
        let mut limiter = PresenceLimiter::new();
        let client = v4(203, 0, 113, 7);
        for _ in 0..PRESENCE_LIMIT {
            assert_eq!(limiter.check(client, T0), Ok(()));
        }
        assert_eq!(limiter.check(client, T0 + 1_500), Err(59));
        // Same /24, other host: same bucket.
        assert!(limiter.check(v4(203, 0, 113, 99), T0).is_err());
        // Another prefix is unaffected.
        assert_eq!(limiter.check(v4(198, 51, 100, 7), T0), Ok(()));
        // The window ends.
        assert_eq!(limiter.check(client, T0 + PRESENCE_WINDOW_MS), Ok(()));
    }

    #[test]
    fn limiter_groups_ipv6_by_56() {
        let mut limiter = PresenceLimiter::new();
        let a = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0x1, 0x00ff, 0, 0, 0, 1));
        let b = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0x1, 0x0001, 9, 9, 9, 9));
        let other = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0x1, 0x0100, 0, 0, 0, 1));
        for _ in 0..PRESENCE_LIMIT {
            limiter.check(a, T0).unwrap();
        }
        assert!(limiter.check(b, T0).is_err());
        assert_eq!(limiter.check(other, T0), Ok(()));
    }

    #[test]
    fn limiter_is_bounded() {
        let mut limiter = PresenceLimiter::new();
        for n in 0..MAX_TRACKED_PREFIXES {
            let ip = v4(10, (n >> 8) as u8, n as u8, 1);
            limiter.check(ip, T0).unwrap();
        }
        assert_eq!(limiter.tracked(), MAX_TRACKED_PREFIXES);
        assert_eq!(limiter.check(v4(11, 0, 0, 1), T0 + 1), Err(60));
        // Once the old windows end, the new prefix gets in and the map is pruned.
        assert_eq!(
            limiter.check(v4(11, 0, 0, 1), T0 + PRESENCE_WINDOW_MS),
            Ok(())
        );
        assert_eq!(limiter.tracked(), 1);
    }

    #[test]
    fn answer_reports_unknown_ids_false_with_no_store_json() {
        let relay = relay();
        let mut limiter = PresenceLimiter::new();
        let (a, b) = (id(1), id(2));
        let query = format!("{a},{b}");
        let response = answer(&mut limiter, &relay, v4(203, 0, 113, 7), Some(&query), T0);
        assert_eq!(response.status, 200);
        assert_eq!(header(&response, "content-type"), Some("application/json"));
        assert_eq!(header(&response, "cache-control"), Some("no-store"));
        let mut expected = Map::new();
        expected.insert(a, Value::Bool(false));
        expected.insert(b, Value::Bool(false));
        assert_eq!(body(&response), Value::Object(expected));
    }

    #[test]
    fn answer_errors_are_400_json() {
        let relay = relay();
        let mut limiter = PresenceLimiter::new();
        let client = v4(203, 0, 113, 7);
        let many = (0..21).map(id).collect::<Vec<_>>().join(",");
        let response = answer(&mut limiter, &relay, client, Some(&many), T0);
        assert_eq!(response.status, 400);
        assert_eq!(body(&response), json!({ "error": "too many ids" }));
        assert_eq!(header(&response, "cache-control"), Some("no-store"));
        let response = answer(&mut limiter, &relay, client, Some("bad"), T0);
        assert_eq!(response.status, 400);
        assert_eq!(body(&response), json!({ "error": "malformed id" }));
        let response = answer(&mut limiter, &relay, client, None, T0);
        assert_eq!(response.status, 400);
    }

    #[test]
    fn answer_429_carries_retry_after() {
        let relay = relay();
        let mut limiter = PresenceLimiter::new();
        let client = v4(203, 0, 113, 7);
        let one = id(1);
        for _ in 0..PRESENCE_LIMIT {
            assert_eq!(
                answer(&mut limiter, &relay, client, Some(&one), T0).status,
                200
            );
        }
        let response = answer(&mut limiter, &relay, client, Some(&one), T0);
        assert_eq!(response.status, 429);
        assert_eq!(header(&response, "retry-after"), Some("60"));
        assert_eq!(header(&response, "cache-control"), Some("no-store"));
    }
}
