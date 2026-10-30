//! `presence::answer` against a relay with a registered host: true while its
//! host socket is open, false after it closes, unknown ids false.

mod relay_harness;

use bp_relay_core::presence::{PresenceLimiter, answer};
use relay_harness::*;
use serde_json::{Value, json};

fn query(h: &Harness, limiter: &mut PresenceLimiter, ids: &str) -> Value {
    let response = answer(limiter, &h.relay, CLIENT, Some(ids), h.now);
    assert_eq!(response.status, 200);
    serde_json::from_slice(&response.body).expect("json")
}

#[test]
fn presence_follows_the_host_socket() {
    let mut h = Harness::new();
    let mut limiter = PresenceLimiter::new();
    let pc = identity(1).id().to_string();
    let unknown = identity(2).id().to_string();
    let ids = format!("{pc},{unknown}");

    assert_eq!(
        query(&h, &mut limiter, &ids),
        json!({ pc.clone(): false, unknown.clone(): false })
    );
    let (host, _) = h.register_host(&identity(1), Some(TOKEN));
    assert_eq!(
        query(&h, &mut limiter, &ids),
        json!({ pc.clone(): true, unknown.clone(): false })
    );
    // Lowercase input answers under the canonical id.
    assert_eq!(
        query(&h, &mut limiter, &pc.to_ascii_lowercase()),
        json!({ pc.clone(): true })
    );
    h.close(host.id);
    assert_eq!(
        query(&h, &mut limiter, &ids),
        json!({ pc.clone(): false, unknown.clone(): false })
    );
}
