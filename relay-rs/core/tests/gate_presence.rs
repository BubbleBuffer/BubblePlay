//! `GET /presence` passes the gate before its navigation step: it never opens
//! an approval, holds, publishes to ntfy or sets a cookie, with or without
//! `Sec-Fetch-Dest`. A foreign Origin is still refused, and every other
//! method on the path keeps the gate's usual answer.

mod gate_support;

use bp_relay_core::gate::{Request, Verdict};
use gate_support::*;

fn presence<'a>(method: &'a str, dest: Option<&'a str>, now: u64) -> Request<'a> {
    let mut request = req(method, "/presence", CLIENT, now);
    request.query = Some("ids=77VH-7M56-8GJK-Y12J");
    request.sec_fetch_dest = dest;
    request
}

#[test]
fn presence_get_is_allowed_without_cookie_and_opens_nothing() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(300);
    for dest in [None, Some("document"), Some("empty")] {
        for n in 0..5 {
            let verdict = navigate_request(&mut gate, &mut rng, &presence("GET", dest, T0 + n));
            assert_eq!(verdict, Verdict::Allow, "dest {dest:?}");
        }
    }
    assert!(gate.pending_ids(T0 + 10).is_empty());
    assert_eq!(gate.snapshot().records.len(), 0);
    // The prefix has no cooldown or pending: a real navigation still opens one.
    let (_, effects) = hold_parts(get(&mut gate, &mut rng, CLIENT, T0 + 20));
    assert_eq!(effects.len(), 1);
}

#[test]
fn presence_with_foreign_origin_is_refused() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(301);
    let mut request = presence("GET", None, T0);
    request.origin = Some("https://evil.example");
    assert_eq!(
        navigate_request(&mut gate, &mut rng, &request),
        Verdict::Refuse
    );
    let mut request = presence("GET", None, T0);
    request.origin = Some(ORIGIN);
    assert_eq!(
        navigate_request(&mut gate, &mut rng, &request),
        Verdict::Allow
    );
}

#[test]
fn other_methods_and_paths_keep_gate_behaviour() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(302);
    let (response, effects) = handled_parts(navigate_request(
        &mut gate,
        &mut rng,
        &presence("POST", None, T0),
    ));
    assert_eq!(response.status, 403);
    assert!(effects.is_empty());
    assert!(is_drop(&navigate_request(
        &mut gate,
        &mut rng,
        &presence("HEAD", None, T0)
    )));
    // Near misses are ordinary gated paths: a cookieless navigation holds.
    for path in ["/presence/", "/presenceX", "/Presence"] {
        let mut gate = new_gate();
        let verdict = navigate_request(&mut gate, &mut rng, &req("GET", path, CLIENT, T0));
        assert!(matches!(verdict, Verdict::Hold(..)), "{path}");
    }
    // A websocket on the path is not a presence query.
    let mut request = presence("GET", None, T0);
    request.websocket = true;
    request.origin = Some(ORIGIN);
    assert_eq!(
        navigate_request(&mut gate, &mut rng, &request),
        Verdict::Refuse
    );
}
