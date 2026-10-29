//! Request classification: Origin x HTTP/WebSocket x path kind x cookie state.

mod gate_support;

use bp_relay_core::gate::{COOKIE_MAX_AGE_SECS, Request, Verdict};
use gate_support::*;

const BOGUS_COOKIE: &str = "__Host-gate=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const COOKIE_MS: u64 = COOKIE_MAX_AGE_SECS * 1000;

fn ws_request<'a>(path: &'a str, origin: Option<&'a str>, cookie: Option<&'a str>) -> Request<'a> {
    Request {
        websocket: true,
        origin,
        cookie,
        ..req("GET", path, CLIENT, T0)
    }
}

#[test]
fn gate_origin_mismatch_http_is_refused() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(1);
    let mut request = req("GET", "/", CLIENT, T0);
    request.origin = Some("https://evil.example");
    assert_eq!(gate.handle(&mut rng, &request), Verdict::Refuse);
}

#[test]
fn gate_origin_mismatch_with_valid_cookie_is_refused() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(2);
    let cookie = approve_client(&mut gate, &mut rng, CLIENT, T0);
    let mut request = req("GET", "/", CLIENT, T0 + 10_000);
    request.origin = Some("https://evil.example");
    request.cookie = Some(&cookie);
    assert_eq!(gate.handle(&mut rng, &request), Verdict::Refuse);
}

#[test]
fn gate_origin_match_with_valid_cookie_is_allowed() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(3);
    let cookie = approve_client(&mut gate, &mut rng, CLIENT, T0);
    let mut request = req("GET", "/", CLIENT, T0 + 10_000);
    request.origin = Some(ORIGIN);
    request.cookie = Some(&cookie);
    assert_eq!(gate.handle(&mut rng, &request), Verdict::Allow);
}

#[test]
fn gate_origin_absent_page_without_cookie_waits() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(4);
    let (page, effects) = wait_parts(get(&mut gate, &mut rng, CLIENT, T0));
    assert_eq!(page.status, 200);
    assert_eq!(effects.len(), 1);
}

#[test]
fn gate_origin_match_page_without_cookie_waits() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(5);
    let mut request = req("GET", "/", CLIENT, T0);
    request.origin = Some(ORIGIN);
    let (_, effects) = wait_parts(gate.handle(&mut rng, &request));
    assert_eq!(effects.len(), 1);
}

#[test]
fn gate_asset_path_without_cookie_waits_like_a_page() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(6);
    let (_, effects) = wait_parts(gate.handle(&mut rng, &req("GET", "/app.js", CLIENT, T0)));
    assert_eq!(effects.len(), 1);
}

#[test]
fn gate_valid_cookie_allows_page() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(7);
    let cookie = approve_client(&mut gate, &mut rng, CLIENT, T0);
    let verdict = get_with_cookie(&mut gate, &mut rng, "/pair", &cookie, T0 + 10_000);
    assert_eq!(verdict, Verdict::Allow);
}

#[test]
fn gate_valid_cookie_allows_asset_path_without_origin() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(8);
    let cookie = approve_client(&mut gate, &mut rng, CLIENT, T0);
    let verdict = get_with_cookie(&mut gate, &mut rng, "/bubbleplay.css", &cookie, T0 + 10_000);
    assert_eq!(verdict, Verdict::Allow);
}

#[test]
fn gate_valid_cookie_is_found_among_other_cookies() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(9);
    let cookie = approve_client(&mut gate, &mut rng, CLIENT, T0);
    let combined = format!("theme=dark; {cookie}; lang=en");
    let mut request = req("GET", "/", CLIENT, T0 + 10_000);
    request.cookie = Some(&combined);
    assert_eq!(gate.handle(&mut rng, &request), Verdict::Allow);
}

#[test]
fn gate_expired_cookie_waits_again() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(10);
    let cookie = approve_client(&mut gate, &mut rng, CLIENT, T0);
    let after = T0 + 2_000 + COOKIE_MS;
    let verdict = get_with_cookie(&mut gate, &mut rng, "/", &cookie, after);
    assert!(matches!(verdict, Verdict::Wait(_, _)), "got {verdict:?}");
}

#[test]
fn gate_unknown_cookie_waits() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(11);
    let verdict = get_with_cookie(&mut gate, &mut rng, "/", BOGUS_COOKIE, T0);
    assert!(matches!(verdict, Verdict::Wait(_, _)), "got {verdict:?}");
}

#[test]
fn gate_post_without_cookie_is_403_and_opens_nothing() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(12);
    let request = req("POST", "/pair", CLIENT, T0);
    let (page, effects) = wait_parts(gate.handle(&mut rng, &request));
    assert_eq!(page.status, 403);
    assert!(effects.is_empty());
    assert!(gate.snapshot().records.is_empty());
    assert_eq!(gate.pending_ids(T0).len(), 0);
}

#[test]
fn gate_head_without_cookie_waits_with_empty_body_and_opens_nothing() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(13);
    let (page, effects) = wait_parts(gate.handle(&mut rng, &req("HEAD", "/", CLIENT, T0)));
    assert_eq!(page.status, 200);
    assert!(page.body.is_empty());
    assert!(effects.is_empty());
    assert_eq!(gate.pending_ids(T0).len(), 0);
}

#[test]
fn gate_unknown_gate_path_is_refused() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(14);
    let request = req("GET", "/__gate/check", CLIENT, T0);
    assert_eq!(gate.handle(&mut rng, &request), Verdict::Refuse);
}

#[test]
fn gate_decision_with_get_is_refused() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(15);
    let request = req("GET", "/__gate/decision", CLIENT, T0);
    assert_eq!(gate.handle(&mut rng, &request), Verdict::Refuse);
}

#[test]
fn gate_poll_with_get_is_refused() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(16);
    let request = req("GET", "/__gate/poll", CLIENT, T0);
    assert_eq!(gate.handle(&mut rng, &request), Verdict::Refuse);
}

#[test]
fn gate_manage_path_is_allowed_without_cookie_or_origin() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(17);
    let request = req("GET", "/__gate/manage/pcs", CLIENT, T0);
    assert_eq!(gate.handle(&mut rng, &request), Verdict::Allow);
}

#[test]
fn gate_manage_path_with_origin_mismatch_is_refused() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(18);
    let mut request = req("GET", "/__gate/manage/pcs", CLIENT, T0);
    request.origin = Some("https://evil.example");
    assert_eq!(gate.handle(&mut rng, &request), Verdict::Refuse);
}

#[test]
fn gate_forget_without_origin_is_refused_even_with_cookie() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(19);
    let cookie = approve_client(&mut gate, &mut rng, CLIENT, T0);
    let mut request = req("POST", "/__gate/forget", CLIENT, T0 + 10_000);
    request.cookie = Some(&cookie);
    assert_eq!(gate.handle(&mut rng, &request), Verdict::Refuse);
}

#[test]
fn gate_forget_with_unknown_cookie_is_refused() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(20);
    let mut request = req("POST", "/__gate/forget", CLIENT, T0);
    request.origin = Some(ORIGIN);
    request.cookie = Some(BOGUS_COOKIE);
    assert_eq!(gate.handle(&mut rng, &request), Verdict::Refuse);
}

#[test]
fn gate_websocket_with_origin_mismatch_is_refused() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(21);
    let request = ws_request("/ws", Some("https://evil.example"), None);
    assert_eq!(gate.handle(&mut rng, &request), Verdict::Refuse);
}

#[test]
fn gate_websocket_with_origin_and_no_cookie_is_refused() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(22);
    let request = ws_request("/ws", Some(ORIGIN), None);
    assert_eq!(gate.handle(&mut rng, &request), Verdict::Refuse);
}

#[test]
fn gate_websocket_with_origin_and_valid_cookie_is_allowed() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(23);
    let cookie = approve_client(&mut gate, &mut rng, CLIENT, T0);
    let request = Request {
        now_ms: T0 + 10_000,
        ..ws_request("/media-ws", Some(ORIGIN), Some(&cookie))
    };
    assert_eq!(gate.handle(&mut rng, &request), Verdict::Allow);
}

#[test]
fn gate_websocket_with_origin_and_expired_cookie_is_refused() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(24);
    let cookie = approve_client(&mut gate, &mut rng, CLIENT, T0);
    let request = Request {
        now_ms: T0 + 2_000 + COOKIE_MS,
        ..ws_request("/file-ws", Some(ORIGIN), Some(&cookie))
    };
    assert_eq!(gate.handle(&mut rng, &request), Verdict::Refuse);
}

#[test]
fn gate_websocket_without_origin_is_allowed_without_cookie() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(25);
    let request = ws_request("/ws", None, None);
    assert_eq!(gate.handle(&mut rng, &request), Verdict::Allow);
}

#[test]
fn gate_websocket_under_gate_path_is_refused() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(26);
    let request = ws_request("/__gate/poll", None, None);
    assert_eq!(gate.handle(&mut rng, &request), Verdict::Refuse);
}

#[test]
fn gate_unknown_cookie_name_is_not_a_gate_cookie() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(27);
    let cookie = approve_client(&mut gate, &mut rng, CLIENT, T0);
    let renamed = cookie.replacen("__Host-gate=", "gate=", 1);
    let verdict = get_with_cookie(&mut gate, &mut rng, "/", &renamed, T0 + 10_000);
    assert!(matches!(verdict, Verdict::Wait(_, _)), "got {verdict:?}");
}
