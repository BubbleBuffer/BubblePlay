//! The wait page, the limit and not-configured pages, and the headers on each
//! gate response.

mod gate_support;

use bp_relay_core::gate::{Gate, GateSnapshot, Response};
use gate_support::*;

const DECIDER: &str = "198.51.100.200";

fn csp_of(response: &Response) -> &str {
    header(response, "Content-Security-Policy").expect("CSP header")
}

fn nonce_in_csp(csp: &str) -> String {
    let start = csp.find("'nonce-").expect("nonce in CSP") + "'nonce-".len();
    let end = start + csp[start..].find('\'').expect("nonce end");
    csp[start..end].to_string()
}

fn body_text(response: &Response) -> String {
    String::from_utf8(response.body.clone()).expect("utf-8 page")
}

fn wait_page(gate: &mut Gate, rng: &mut TestRng, client: &str, now: u64) -> Response {
    wait_parts(get(gate, rng, client, now)).0
}

#[test]
fn gate_wait_page_sends_the_strict_csp_and_security_headers() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(400);
    let page = wait_page(&mut gate, &mut rng, CLIENT, T0);
    assert_eq!(page.status, 200);
    assert_eq!(
        header(&page, "Content-Type"),
        Some("text/html; charset=utf-8")
    );
    assert_eq!(header(&page, "X-Frame-Options"), Some("DENY"));
    assert_eq!(header(&page, "Cache-Control"), Some("no-store"));
    assert_eq!(header(&page, "X-Content-Type-Options"), Some("nosniff"));
    assert_eq!(header(&page, "Referrer-Policy"), Some("no-referrer"));
    let csp = csp_of(&page);
    assert!(csp.contains("default-src 'none'"));
    assert!(csp.contains("connect-src 'self'"));
    assert!(csp.contains("frame-ancestors 'none'"));
    assert!(!csp.contains("unsafe-inline"));
    assert!(!csp.contains("unsafe-eval"));
}

#[test]
fn gate_wait_page_nonce_matches_the_csp_and_changes_per_response() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(401);
    let first = wait_page(&mut gate, &mut rng, CLIENT, T0);
    let nonce = nonce_in_csp(csp_of(&first));
    let body = body_text(&first);
    assert!(body.contains(&format!("<style nonce=\"{nonce}\">")));
    assert!(body.contains(&format!("<script nonce=\"{nonce}\">")));
    let second = wait_page(&mut gate, &mut rng, "10.0.0.1", T0);
    assert_ne!(nonce, nonce_in_csp(csp_of(&second)));
}

#[test]
fn gate_wait_page_is_minimal_and_unbranded() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(402);
    let body = body_text(&wait_page(&mut gate, &mut rng, CLIENT, T0));
    assert!(body.contains("Waiting for approval"));
    assert!(!body.contains("BubblePlay"));
    assert!(!body.contains("<title"));
    assert!(!body.contains("src="));
    assert!(!body.contains("href="));
}

#[test]
fn gate_wait_page_polls_the_gate_endpoint_and_reloads_on_approval() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(403);
    let body = body_text(&wait_page(&mut gate, &mut rng, CLIENT, T0));
    assert!(body.contains("\"/__gate/poll\""));
    assert!(body.contains("location.reload()"));
    assert!(body.contains("Not approved."));
}

#[test]
fn gate_wait_page_carries_a_different_poll_token_per_approval() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(404);
    let first = poll_token(&wait_page(&mut gate, &mut rng, CLIENT, T0));
    let second = poll_token(&wait_page(&mut gate, &mut rng, "10.0.0.1", T0));
    assert_ne!(first, second);
}

#[test]
fn gate_poll_response_carries_json_and_the_security_headers() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(405);
    let page = wait_page(&mut gate, &mut rng, CLIENT, T0);
    let token = poll_token(&page);
    let (response, _) = handled_parts(poll(&mut gate, &mut rng, &token, CLIENT, T0 + 1_000));
    assert_eq!(response.status, 200);
    assert_eq!(header(&response, "Content-Type"), Some("application/json"));
    assert_eq!(header(&response, "Cache-Control"), Some("no-store"));
    assert_eq!(header(&response, "X-Content-Type-Options"), Some("nosniff"));
    assert_eq!(header(&response, "X-Frame-Options"), Some("DENY"));
    assert_eq!(header(&response, "Referrer-Policy"), Some("no-referrer"));
    assert_eq!(state_of(&response), "pending");
}

#[test]
fn gate_try_later_page_is_429_html_with_the_same_csp() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(406);
    let _ = wait_page(&mut gate, &mut rng, CLIENT, T0);
    let limited = wait_parts(get(&mut gate, &mut rng, CLIENT, T0)).0;
    assert_eq!(limited.status, 429);
    assert_eq!(header(&limited, "X-Frame-Options"), Some("DENY"));
    assert!(csp_of(&limited).contains("default-src 'none'"));
    let body = body_text(&limited);
    assert!(body.contains("Try again later."));
    assert!(!body.contains("<script"));
}

#[test]
fn gate_not_configured_page_is_503_with_csp_and_no_script() {
    let mut gate = Gate::new(config(false), GateSnapshot::default(), T0).expect("valid config");
    let mut rng = TestRng::new(407);
    let page = wait_page(&mut gate, &mut rng, CLIENT, T0);
    assert_eq!(page.status, 503);
    assert!(csp_of(&page).contains("default-src 'none'"));
    let body = body_text(&page);
    assert!(body.contains("Approval not configured."));
    assert!(!body.contains("<script"));
}

#[test]
fn gate_decision_answers_204_with_no_store_and_no_body() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(408);
    let _ = wait_page(&mut gate, &mut rng, CLIENT, T0);
    let (response, _) = handled_parts(decide(
        &mut gate,
        &mut rng,
        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "approve",
        DECIDER,
        T0 + 1_000,
    ));
    assert_eq!(response.status, 204);
    assert!(response.body.is_empty());
    assert_eq!(header(&response, "Cache-Control"), Some("no-store"));
    assert_eq!(header(&response, "X-Content-Type-Options"), Some("nosniff"));
}

#[test]
fn gate_forget_response_has_the_security_headers() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(409);
    let cookie = approve_client(&mut gate, &mut rng, CLIENT, T0);
    let mut request = req("POST", "/__gate/forget", CLIENT, T0 + 10_000);
    request.origin = Some(ORIGIN);
    request.cookie = Some(&cookie);
    let (response, _) = handled_parts(gate.handle(&mut rng, &request));
    assert_eq!(header(&response, "Cache-Control"), Some("no-store"));
    assert_eq!(header(&response, "Referrer-Policy"), Some("no-referrer"));
}

#[test]
fn gate_head_wait_page_keeps_its_headers_and_drops_the_body() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(410);
    let (page, _) = wait_parts(gate.handle(&mut rng, &req("HEAD", "/", CLIENT, T0)));
    assert!(page.body.is_empty());
    assert!(csp_of(&page).contains("default-src 'none'"));
}
