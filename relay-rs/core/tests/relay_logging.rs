//! Relay log lines: every refusal and close is logged with a status, a reason and a
//! class, ids are cut to 8 characters, and secrets never reach a line.

mod relay_harness;

use bp_relay_core::log::capture;
use bp_relay_core::{Relay, Route};
use relay_harness::*;
use serde_json::{Value, json};

const PAIR: [u8; 32] = [42; 32];
const WRONG_PAIR: [u8; 32] = [43; 32];

fn host_up(h: &mut Harness) -> Host {
    h.register_host(&identity(1), Some(TOKEN)).0
}

fn parse(lines: &[String]) -> Vec<Value> {
    lines
        .iter()
        .map(|line| serde_json::from_str(line).expect("every log line is one JSON object"))
        .collect()
}

/// The lines whose `fields.class` is `class`.
fn with_class<'a>(lines: &'a [Value], class: &str) -> Vec<&'a Value> {
    lines
        .iter()
        .filter(|line| line["fields"]["class"] == class)
        .collect()
}

#[test]
fn wrong_pairing_code_is_logged_with_class_status_and_reason() {
    let mut h = Harness::new();
    let host = host_up(&mut h);
    h.text(host.id, &json!({"type": "pair-open", "admit": b64(&PAIR)}));
    let (_, lines) = capture(|| {
        let (id, actions) = h.viewer_join(&identity(201), &host.pc, false, Some(&WRONG_PAIR));
        assert_eq!(
            closed(&actions, id),
            Some((1008, "pairing code not accepted".to_owned()))
        );
    });
    let lines = parse(&lines);
    let refused = with_class(&lines, "pairing.wrong-code");
    assert_eq!(refused.len(), 1, "{lines:?}");
    let line = refused[0];
    assert_eq!(line["src"], "relay");
    assert_eq!(line["level"], "warn");
    assert_eq!(line["fields"]["status"], 1008);
    assert_eq!(line["fields"]["reason"], "pairing code not accepted");
    assert_eq!(line["fields"]["role"], "viewer");
    // Ids are cut to their first 8 characters.
    assert_eq!(line["pc"], host.pc[..8]);
    assert_eq!(line["fields"]["peer"], identity(201).id().to_string()[..8]);
    assert!(!line.to_string().contains(&host.pc));
}

#[test]
fn each_refusal_site_logs_its_own_class_and_reason() {
    let mut h = Harness::new();
    let host = host_up(&mut h);
    let (_, lines) = capture(|| {
        // Not on the admission list, no pairing code.
        let (id, actions) = h.viewer_join(&identity(201), &host.pc, false, None);
        assert_eq!(closed(&actions, id), Some((1008, "not paired".to_owned())));
        // A viewer that never says hello.
        let silent = h.upgrade(req(Route::Ws, Some(&host.pc), None, true));
        let out = h.advance(10_000);
        assert_eq!(closed(&out, silent), Some((1008, "timed out".to_owned())));
    });
    let lines = parse(&lines);
    let not_paired = with_class(&lines, "viewer.not-admitted");
    assert_eq!(not_paired.len(), 1, "{lines:?}");
    assert_eq!(not_paired[0]["fields"]["reason"], "not paired");
    let timed_out = with_class(&lines, "timeout.hello");
    assert_eq!(timed_out.len(), 1, "{lines:?}");
    assert_eq!(timed_out[0]["fields"]["reason"], "timed out");
}

#[test]
fn ordinary_closes_are_logged_at_info() {
    let mut h = Harness::new();
    let host = host_up(&mut h);
    let viewer = h.open_viewer(&host.pc);
    let (_, lines) = capture(|| {
        let out = h.text(viewer.id, &json!({"type": "bye"}));
        assert_eq!(closed(&out, viewer.id), Some((1000, "bye".to_owned())));
    });
    let lines = parse(&lines);
    let bye = with_class(&lines, "bye");
    assert_eq!(bye.len(), 1, "{lines:?}");
    assert_eq!(bye[0]["level"], "info");
    assert_eq!(bye[0]["fields"]["status"], 1000);
}

#[test]
fn rejected_upgrades_are_logged() {
    let mut h = Harness::new();
    let (_, lines) = capture(|| {
        let refused = h.try_upgrade(req(Route::Ws, Some("not-a-device-id"), None, true));
        assert!(refused.is_err());
    });
    let lines = parse(&lines);
    let line = with_class(&lines, "upgrade.bad-pc");
    assert_eq!(line.len(), 1, "{lines:?}");
    assert_eq!(line[0]["fields"]["status"], 404);
    assert!(line[0]["pc"].is_null());
    assert!(!line[0].to_string().contains("not-a-device-id"));
}

#[test]
fn secrets_sent_by_peers_never_appear_in_log_lines() {
    let mut h = Harness::new();
    let host = host_up(&mut h);
    let wrong_token = "wrong-token-SECRET-0123456789";
    let (_, lines) = capture(|| {
        // A host that presents a wrong token.
        let (_, actions) = h.host_hello(&identity(2), Some(wrong_token));
        assert!(!actions.is_empty());
        // A pairing viewer that presents a wrong pairing value.
        h.text(host.id, &json!({"type": "pair-open", "admit": b64(&PAIR)}));
        h.viewer_join(&identity(201), &host.pc, false, Some(&WRONG_PAIR));
    });
    let all = lines.join("\n");
    assert!(!lines.is_empty());
    for secret in [
        wrong_token,
        TOKEN,
        b64(&PAIR).as_str(),
        b64(&WRONG_PAIR).as_str(),
    ] {
        assert!(!all.contains(secret), "{secret} leaked into the log");
    }
    // The configured token is also registered as a literal, so even a message that
    // quoted it would be scrubbed.
    bp_relay_core::log::register_secret(TOKEN);
    let (_, lines) = capture(|| {
        bp_relay_core::log::emit(
            bp_relay_core::log::Level::Info,
            0,
            "relay",
            &format!("config says {TOKEN}"),
            None,
            json!({"note": TOKEN}),
        );
    });
    assert!(!lines.join("").contains(TOKEN));
}

#[test]
fn dropped_client_log_messages_are_counted_and_reported_once_a_minute() {
    let mut h = Harness::new();
    let host = host_up(&mut h);
    let viewer = h.open_viewer(&host.pc);
    let client_log = json!({
        "type": "client-log", "seq": 1, "level": "error", "event": "decoder-error",
        "detail": {"message": "PRIVATE-CONTENT-123"},
    });
    let (_, lines) = capture(|| {
        for _ in 0..5 {
            let out = h.text(viewer.id, &client_log);
            // Dropped: nothing is forwarded and the socket stays open.
            assert!(
                out.iter()
                    .all(|a| !matches!(a, bp_relay_core::Action::Close(..)))
            );
            assert!(sent_text(&out, host.id).is_empty());
        }
        h.now += 60_000;
        h.text(viewer.id, &client_log);
    });
    let all = lines.join("\n");
    assert!(
        !all.contains("PRIVATE-CONTENT-123"),
        "contents must not be logged"
    );
    let lines = parse(&lines);
    let dropped = with_class(&lines, "client-log.dropped");
    assert_eq!(dropped.len(), 2, "{lines:?}");
    assert_eq!(dropped[0]["level"], "debug");
    assert_eq!(dropped[0]["fields"]["count"], 1);
    assert_eq!(dropped[1]["fields"]["count"], 5);
}

#[test]
fn rehydration_refusals_are_logged_with_a_reason() {
    let mut h = Harness::new();
    let host = host_up(&mut h);
    let (viewer, _) = h.viewer_hello(&host.pc, false);
    let mut snapshot = h.relay.snapshot_of(viewer.id).expect("viewer is open");
    snapshot.epoch = "bad epoch".to_owned();
    let (_, lines) = capture(|| {
        let (_, out) = Relay::rehydrate(
            h.relay.config().clone(),
            h.relay.pcs().clone(),
            vec![snapshot],
            h.now,
        );
        assert_eq!(
            closed(&out, viewer.id),
            Some((1008, "session not restored".to_owned()))
        );
    });
    let lines = parse(&lines);
    let line = with_class(&lines, "restore.invalid");
    assert_eq!(line.len(), 1, "{lines:?}");
    assert_eq!(line[0]["fields"]["reason"], "session not restored");
}
