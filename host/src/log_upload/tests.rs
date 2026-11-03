use super::*;
use crate::client_logs::parse_ts_strict;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::UNIX_EPOCH;

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct TempDir(PathBuf);
impl TempDir {
    fn new(tag: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("bp-log-upload-{tag}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "../../../fixtures/client-log-upload-contract.json"
    ))
    .unwrap()
}

fn message(name: &str) -> Value {
    fixture()["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == name)
        .unwrap_or_else(|| panic!("fixture message {name}"))["json"]
        .clone()
}

/// 2026-11-02T14:00:00Z: the day of the fixture lines, so retention never drops them.
fn fixed_now() -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(parse_ts_strict("2026-11-02T14:00:00.000Z").unwrap() as u64)
}

fn limits() -> UploadLimits {
    UploadLimits {
        clock: fixed_now,
        ..UploadLimits::default()
    }
}

fn upload(root: &Path) -> LogUpload {
    LogUpload::open_with(root, limits()).unwrap()
}

/// A started, capability-negotiated connection for `device` with the initial `logs-state` consumed.
fn connected(root: &Path, device: &str) -> LogUpload {
    let mut up = upload(root);
    up.start(device, "Test phone", true);
    let state = drain(&mut up, 1);
    assert_eq!(state[0]["type"], "logs-state");
    up
}

fn drain(up: &mut LogUpload, want: usize) -> Vec<Value> {
    let start = Instant::now();
    let mut out = Vec::new();
    while out.len() < want {
        out.extend(up.poll());
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "timed out waiting for {want} answers, have {out:?}"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    out
}

fn send(up: &mut LogUpload, kind: &str, msg: &Value) -> Result<Vec<Value>, Ignored> {
    up.on_message(kind, &msg.to_string())
}

fn line(src: &str, ts: &str, msg: &str) -> String {
    json!({"ts": ts, "level": "info", "src": src, "device": "someone-else", "pc": null,
           "session": null, "target": "t", "msg": msg, "fields": {}})
    .to_string()
}

fn cur(n: u64) -> String {
    format!("{n:020}")
}

fn batch(stream: &str, id: u64, from: u64, to: u64, lines: &[String]) -> Value {
    json!({"type":"logs-batch","v":1,"stream":stream,"id":id,"from":cur(from),"to":cur(to),
           "live":false,"lines":lines})
}

fn report(id: &str) -> Value {
    json!({"version":1,"id":id,"created":"2026-11-02T13:50:00.000Z","text":"Stream froze",
           "kind":"bug","app":{"version":"0.3.0","build":"42","commit":"abc1234"},
           "platform":"android","device":{"id":"victim","name":"Pixel","model":"Pixel 8"},
           "pc":"pc-1","session":null,"route":"session","locale":"en-US","logs":[],"errors":[],
           "screenshot":null})
}

fn png(len: usize) -> Vec<u8> {
    let mut v = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    v.extend((0..len.saturating_sub(8)).map(|i| (i % 251) as u8));
    v
}

fn begin(id: &str, png_len: usize) -> Value {
    json!({"type":"feedback-begin","v":1,"id":id,"report":report(id),"pngBytes":png_len,
           "chunks":png_len.div_ceil(MAX_FEEDBACK_CHUNK_BYTES)})
}

fn chunk(id: &str, i: u64, bytes: &[u8]) -> Value {
    json!({"type":"feedback-chunk","v":1,"id":id,"i":i,"data":STANDARD.encode(bytes)})
}

/// Sends a whole report; returns the answers (the ack arrives through poll).
fn upload_report(up: &mut LogUpload, id: &str, png_bytes: &[u8]) -> Vec<Value> {
    let mut immediate = send(up, "feedback-begin", &begin(id, png_bytes.len())).unwrap();
    for (i, part) in png_bytes.chunks(MAX_FEEDBACK_CHUNK_BYTES).enumerate() {
        immediate.extend(send(up, "feedback-chunk", &chunk(id, i as u64, part)).unwrap());
    }
    immediate
}

fn read_all(dir: &Path) -> String {
    let mut text = String::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                text.push_str(&read_all(&path));
            } else if let Ok(body) = fs::read(&path) {
                text.push_str(&String::from_utf8_lossy(&body));
            }
        }
    }
    text
}

fn files_in(dir: &Path, ext: &str) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.ends_with(ext) && !n.starts_with('.'))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

// ---------------------------------------------------------------- fixture

#[test]
fn host_to_client_messages_encode_exactly_the_fixture_json() {
    let fx = fixture();
    assert_eq!(fx["capability"], CAP_CLIENT_LOGS);
    let l = &fx["limits"];
    assert_eq!(l["maxBatchBytes"], MAX_BATCH_BYTES);
    assert_eq!(l["maxBatchLines"], MAX_BATCH_LINES);
    assert_eq!(l["connectionBudgetBytes"], CONNECTION_BUDGET_BYTES);
    assert_eq!(l["minBatchIntervalMs"], MIN_BATCH_INTERVAL_MS);
    assert_eq!(l["maxFeedbackPngBytes"], MAX_FEEDBACK_PNG_BYTES);
    assert_eq!(l["maxFeedbackChunkBytes"], MAX_FEEDBACK_CHUNK_BYTES);

    let at42 = "2026-11-02T13:45:12.345Z#000042".to_string();
    let at44 = "2026-11-02T13:45:13.001Z#000044";
    let state: BTreeMap<String, String> = [("app-android-core".to_string(), at42)].into();
    assert_eq!(
        logs_state_message(&state, &UploadLimits::default()),
        message("logs-state")
    );
    let stream = "app-android-core";
    assert_eq!(
        logs_ack_message(stream, 17, LogsStatus::Ok, at44, 0, 0, None),
        message("logs-ack-ok")
    );
    assert_eq!(
        logs_ack_message(stream, 18, LogsStatus::RateLimited, at44, 0, 2000, None),
        message("logs-ack-rate-limited")
    );
    assert_eq!(
        logs_ack_message(stream, 19, LogsStatus::Budget, at44, 0, 0, None),
        message("logs-ack-budget")
    );
    assert_eq!(
        logs_ack_message(
            stream,
            20,
            LogsStatus::Rejected,
            at44,
            0,
            0,
            Some("batch too large")
        ),
        message("logs-ack-rejected")
    );
    assert_eq!(
        feedback_ack_message(
            "7b1f0c6e-3d2a-4c55-9a58-0f1d2b3c4d5e",
            FeedbackStatus::Ok,
            ""
        ),
        message("feedback-ack-ok")
    );
}

#[test]
fn client_to_host_messages_decode() {
    let batch: LogsBatchMsg = serde_json::from_value(message("logs-batch")).unwrap();
    assert_eq!(batch.stream, "app-android-core");
    assert_eq!(batch.id, 17);
    assert_eq!(batch.from, "2026-11-02T13:45:12.345Z#000042");
    assert_eq!(batch.to, "2026-11-02T13:45:13.001Z#000044");
    assert!(!batch.live);
    assert_eq!(batch.lines.len(), 2);
    let begin: FeedbackBeginMsg = serde_json::from_value(message("feedback-begin")).unwrap();
    assert_eq!(begin.id, "7b1f0c6e-3d2a-4c55-9a58-0f1d2b3c4d5e");
    assert_eq!((begin.png_bytes, begin.chunks), (69, 1));
    assert_eq!(begin.report["kind"], "bug");
    let chunk: FeedbackChunkMsg = serde_json::from_value(message("feedback-chunk")).unwrap();
    assert_eq!(chunk.i, 0);
    assert!(chunk.data.starts_with("iVBORw0KGgo"));
    // Unknown extra fields are ignored.
    let mut extra = message("feedback-chunk");
    extra["future"] = json!({"x": 1});
    assert!(serde_json::from_value::<FeedbackChunkMsg>(extra).is_ok());
}

#[test]
fn fixture_batch_is_stored_acked_and_resumes_across_a_reconnect() {
    let tmp = TempDir::new("fixture");
    let mut up = upload(&tmp.0);
    up.start("dev-1", "Test phone", true);
    let state = drain(&mut up, 1);
    assert_eq!(state[0]["type"], "logs-state");
    assert_eq!(state[0]["v"], 1);
    assert_eq!(state[0]["cursors"], json!({}));
    assert_eq!(state[0]["limits"], message("logs-state")["limits"]);

    let replies = send(&mut up, "logs-batch", &message("logs-batch")).unwrap();
    assert!(replies.is_empty(), "the ack comes from the worker");
    let acks = drain(&mut up, 1);
    assert_eq!(acks[0], message("logs-ack-ok"));
    let stored = fs::read_to_string(tmp.0.join("clients/dev-1/2026-11-02.jsonl")).unwrap();
    assert_eq!(stored.lines().count(), 2);
    for l in stored.lines() {
        let v: Value = serde_json::from_str(l).unwrap();
        assert_eq!(
            v["device"], "dev-1",
            "the host stamps the authenticated device"
        );
    }
    drop(up);

    // A new connection (and a new process): the state carries the stored cursor.
    let mut up = upload(&tmp.0);
    up.start("dev-1", "Test phone", true);
    let state = drain(&mut up, 1);
    assert_eq!(
        state[0]["cursors"],
        json!({"app-android-core": "2026-11-02T13:45:13.001Z#000044"})
    );
    // The client resends the batch whose ack it missed: ok, nothing written twice.
    send(&mut up, "logs-batch", &message("logs-batch")).unwrap();
    let acks = drain(&mut up, 1);
    assert_eq!(acks[0], message("logs-ack-ok"));
    let stored = fs::read_to_string(tmp.0.join("clients/dev-1/2026-11-02.jsonl")).unwrap();
    assert_eq!(stored.lines().count(), 2);
    // Another device has its own cursor.
    up.start("dev-2", "Test phone", true);
    assert_eq!(drain(&mut up, 1)[0]["cursors"], json!({}));
}

#[test]
fn fixture_feedback_messages_are_processed() {
    let tmp = TempDir::new("fixture-fb");
    let mut up = connected(&tmp.0, "dev-1");
    assert!(send(&mut up, "feedback-begin", &message("feedback-begin"))
        .unwrap()
        .is_empty());
    assert!(send(&mut up, "feedback-chunk", &message("feedback-chunk"))
        .unwrap()
        .is_empty());
    assert_eq!(drain(&mut up, 1)[0], message("feedback-ack-ok"));
    let saved = files_in(&tmp.0.join("feedback"), ".json");
    assert_eq!(saved.len(), 1);
    assert!(saved[0].ends_with("-dev-1.json"), "{saved:?}");
    let png_files = files_in(&tmp.0.join("feedback"), ".png");
    assert_eq!(png_files.len(), 1);
    assert_eq!(
        fs::read(tmp.0.join("feedback").join(&png_files[0]))
            .unwrap()
            .len(),
        69
    );
}

// ---------------------------------------------------------------- gating

#[test]
fn messages_before_authentication_are_refused_and_counted() {
    let tmp = TempDir::new("unauth");
    let mut up = upload(&tmp.0);
    let b = batch(
        "app-win",
        1,
        0,
        1,
        &[line("app-win", "2026-11-02T13:00:00.000Z", "x")],
    );
    assert!(send(&mut up, "logs-batch", &b).is_err());
    assert!(send(&mut up, "feedback-begin", &begin("abc", 0)).is_err());
    assert_eq!(up.ignored(), 2);
    assert!(up.poll().is_empty());
    assert!(!tmp.0.join("clients").read_dir().unwrap().any(|_| true));
    // Once the viewer is gone the state is gone too.
    up.start("dev-1", "Test phone", true);
    drain(&mut up, 1);
    up.stop();
    assert!(send(&mut up, "logs-batch", &b).is_err());
}

#[test]
fn capability_off_is_ignored_and_nothing_is_stored_or_sent() {
    let tmp = TempDir::new("capoff");
    let mut up = upload(&tmp.0);
    up.start("dev-1", "Test phone", false);
    assert!(!up.enabled());
    let b = batch(
        "app-win",
        1,
        0,
        1,
        &[line("app-win", "2026-11-02T13:00:00.000Z", "x")],
    );
    assert!(send(&mut up, "logs-batch", &b).is_err());
    assert!(send(&mut up, "feedback-begin", &begin("abc", 0)).is_err());
    std::thread::sleep(Duration::from_millis(50));
    assert!(up.poll().is_empty(), "no logs-state without the capability");
    assert_eq!(up.ignored(), 2);
    assert!(files_in(&tmp.0.join("clients/dev-1"), ".jsonl").is_empty());
    // The capability can turn on later in the session (the viewer's ctl caps message).
    up.offer();
    up.offer();
    let state = drain(&mut up, 1);
    assert_eq!(state[0]["type"], "logs-state");
    std::thread::sleep(Duration::from_millis(50));
    assert!(up.poll().is_empty(), "logs-state is sent once");
}

#[test]
fn unknown_version_and_host_kinds_are_ignored() {
    let tmp = TempDir::new("version");
    let mut up = connected(&tmp.0, "dev-1");
    let mut b = batch(
        "app-win",
        1,
        0,
        1,
        &[line("app-win", "2026-11-02T13:00:00.000Z", "x")],
    );
    b["v"] = json!(2);
    assert!(send(&mut up, "logs-batch", &b).is_err());
    b["v"] = json!("1");
    assert!(send(&mut up, "logs-batch", &b).is_err());
    b.as_object_mut().unwrap().remove("v");
    assert!(send(&mut up, "logs-batch", &b).is_err());
    assert!(up.on_message("logs-batch", "not json").is_err());
    // Host-to-client kinds coming from a client are not accepted.
    assert!(send(&mut up, "logs-ack", &message("logs-ack-ok")).is_err());
    assert!(send(&mut up, "logs-state", &message("logs-state")).is_err());
    assert!(send(&mut up, "feedback-ack", &message("feedback-ack-ok")).is_err());
    assert_eq!(up.ignored(), 7);
    assert!(files_in(&tmp.0.join("clients/dev-1"), ".jsonl").is_empty());
}

#[test]
fn only_kinds_of_this_module_are_routed() {
    for kind in [
        "logs-state",
        "logs-batch",
        "logs-ack",
        "feedback-begin",
        "feedback-chunk",
        "feedback-ack",
    ] {
        assert!(is_upload_kind(kind));
    }
    for kind in ["caps", "input", "offer", "client-log", "logs", ""] {
        assert!(!is_upload_kind(kind));
    }
}

// ---------------------------------------------------------------- logs

#[test]
fn batches_are_validated_before_they_reach_the_store() {
    let tmp = TempDir::new("validate");
    let mut up = connected(&tmp.0, "dev-1");
    let l = line("app-win", "2026-11-02T13:00:00.000Z", "x");
    let reject = |up: &mut LogUpload, b: Value, reason: &str| {
        let replies = send(up, "logs-batch", &b).unwrap();
        assert_eq!(replies.len(), 1, "{b}");
        assert_eq!(replies[0]["status"], "rejected", "{b}");
        assert_eq!(replies[0]["reason"], reason);
        assert_eq!(replies[0]["id"], b["id"]);
    };
    reject(
        &mut up,
        batch("host", 1, 0, 1, std::slice::from_ref(&l)),
        "unknown stream",
    );
    reject(
        &mut up,
        batch("../x", 2, 0, 1, std::slice::from_ref(&l)),
        "unknown stream",
    );
    let mut bad_cursor = batch("app-win", 3, 0, 1, std::slice::from_ref(&l));
    bad_cursor["to"] = json!("has space");
    reject(&mut up, bad_cursor, "invalid cursor");
    reject(
        &mut up,
        batch("app-win", 4, 5, 4, std::slice::from_ref(&l)),
        "invalid cursor",
    );
    let many: Vec<String> = (0..=MAX_BATCH_LINES).map(|_| l.clone()).collect();
    reject(
        &mut up,
        batch("app-win", 5, 0, 101, &many),
        "batch too large",
    );
    let big = line("app-win", "2026-11-02T13:00:00.000Z", &"w".repeat(7000));
    reject(
        &mut up,
        batch("app-win", 6, 0, 3, &[big.clone(), big.clone(), big]),
        "batch too large",
    );
    // A batch that cannot be read at all but names its stream and id still gets an answer.
    let malformed = json!({"type":"logs-batch","v":1,"stream":"app-win","id":7,"lines":"nope"});
    reject(&mut up, malformed, "malformed batch");
    // One without an id is dropped silently (counted).
    let nameless = json!({"type":"logs-batch","v":1,"lines":[]});
    assert!(send(&mut up, "logs-batch", &nameless).is_err());
    assert!(files_in(&tmp.0.join("clients/dev-1"), ".jsonl").is_empty());
    // Within the limits a batch is accepted.
    send(&mut up, "logs-batch", &batch("app-win", 8, 0, 1, &[l])).unwrap();
    assert_eq!(drain(&mut up, 1)[0]["status"], "ok");
}

#[test]
fn lines_of_another_src_are_dropped_and_counted_in_the_ack() {
    let tmp = TempDir::new("srcmatch");
    let mut up = connected(&tmp.0, "dev-1");
    let lines = [
        line("app-win", "2026-11-02T13:00:00.000Z", "mine"),
        line("host", "2026-11-02T13:00:01.000Z", "forged host line"),
        "not json".to_string(),
    ];
    send(&mut up, "logs-batch", &batch("app-win", 1, 0, 3, &lines)).unwrap();
    let ack = drain(&mut up, 1);
    assert_eq!(ack[0]["status"], "ok");
    assert_eq!(ack[0]["dropped"], 2);
    let stored = read_all(&tmp.0.join("clients"));
    assert!(stored.contains("mine") && !stored.contains("forged host line"));
}

#[test]
fn connection_budget_is_enforced_with_a_budget_ack() {
    let tmp = TempDir::new("budget");
    let l = line("app-win", "2026-11-02T13:00:00.000Z", "budget line");
    let size = l.len() as u64 + 1;
    let mut up = LogUpload::open_with(
        &tmp.0,
        UploadLimits {
            connection_budget_bytes: size * 3,
            ..limits()
        },
    )
    .unwrap();
    up.start("dev-1", "Test phone", true);
    let state = drain(&mut up, 1);
    // The announced limit follows the configured budget.
    assert_eq!(state[0]["limits"]["connectionBudgetBytes"], size * 3);
    send(
        &mut up,
        "logs-batch",
        &batch("app-win", 1, 0, 2, &[l.clone(), l.clone()]),
    )
    .unwrap();
    assert_eq!(drain(&mut up, 1)[0]["status"], "ok");
    // One line still fits, a second batch of two does not.
    let over = send(
        &mut up,
        "logs-batch",
        &batch("app-win", 2, 2, 4, &[l.clone(), l.clone()]),
    )
    .unwrap();
    assert_eq!(over.len(), 1, "budget answers are immediate");
    assert_eq!(over[0]["status"], "budget");
    assert_eq!(over[0]["id"], 2);
    assert_eq!(
        over[0]["cursor"],
        cur(2),
        "the ack carries the stored cursor"
    );
    send(
        &mut up,
        "logs-batch",
        &batch("app-win", 3, 2, 3, std::slice::from_ref(&l)),
    )
    .unwrap();
    assert_eq!(drain(&mut up, 1)[0]["status"], "ok");
    let again = send(
        &mut up,
        "logs-batch",
        &batch("app-win", 4, 3, 4, std::slice::from_ref(&l)),
    )
    .unwrap();
    assert_eq!(again[0]["status"], "budget");
    let stored = fs::read_to_string(tmp.0.join("clients/dev-1/2026-11-02.jsonl")).unwrap();
    assert_eq!(stored.lines().count(), 3);
    // The next connection has a fresh budget.
    up.start("dev-1", "Test phone", true);
    drain(&mut up, 1);
    send(&mut up, "logs-batch", &batch("app-win", 1, 3, 4, &[l])).unwrap();
    assert_eq!(drain(&mut up, 1)[0]["status"], "ok");
}

#[test]
fn a_full_queue_answers_rate_limited_and_the_retry_succeeds() {
    let tmp = TempDir::new("queue");
    let mut up = LogUpload::open_with(
        &tmp.0,
        UploadLimits {
            queue_capacity: 1,
            ..limits()
        },
    )
    .unwrap();
    up.start("dev-1", "Test phone", true);
    drain(&mut up, 1);
    let pause = up.pause_handle();
    let guard = pause.lock().unwrap();
    let mut immediate = Vec::new();
    for n in 0..4u64 {
        let l = line("app-win", "2026-11-02T13:00:00.000Z", &format!("line {n}"));
        immediate
            .extend(send(&mut up, "logs-batch", &batch("app-win", n, n, n + 1, &[l])).unwrap());
    }
    // The worker holds one job and the queue one: at least two of the four are refused at once.
    assert!(immediate.len() >= 2, "{immediate:?}");
    for reply in &immediate {
        assert_eq!(reply["status"], "rate-limited");
        assert_eq!(reply["retryAfterMs"], 2000);
    }
    let accepted = 4 - immediate.len();
    drop(guard);
    let acks = drain(&mut up, accepted);
    assert!(acks.iter().all(|a| a["status"] == "ok"));
    // The refused batch is retried unchanged and goes through.
    let refused = immediate[0]["id"].as_u64().unwrap();
    let cursor = up.conn.as_ref().unwrap().cursors["app-win"].clone();
    let l = line("app-win", "2026-11-02T13:00:00.000Z", "retry");
    let mut again = batch("app-win", refused, 0, 0, &[l]);
    again["from"] = json!(cursor);
    again["to"] = json!(cur(100));
    send(&mut up, "logs-batch", &again).unwrap();
    assert_eq!(drain(&mut up, 1)[0]["status"], "ok");
}

#[test]
fn daily_cap_of_the_store_becomes_a_rate_limited_ack() {
    let tmp = TempDir::new("daily");
    let logs = ClientLogStore::open_with(
        &tmp.0,
        crate::client_logs::Limits {
            daily_bytes: 400,
            ..Default::default()
        },
    )
    .unwrap();
    let feedback = FeedbackStore::open(&tmp.0).unwrap();
    let mut up = LogUpload::with_stores(logs, feedback, limits());
    up.start("dev-1", "Test phone", true);
    drain(&mut up, 1);
    let l = line("app-win", "2026-11-02T13:00:00.000Z", &"word ".repeat(30));
    send(
        &mut up,
        "logs-batch",
        &batch("app-win", 1, 0, 2, &[l.clone(), l.clone()]),
    )
    .unwrap();
    assert_eq!(drain(&mut up, 1)[0]["status"], "rate-limited");
    send(&mut up, "logs-batch", &batch("app-win", 2, 0, 1, &[l])).unwrap();
    let ack = drain(&mut up, 1);
    assert_eq!(ack[0]["status"], "ok");
}

#[test]
fn planted_secrets_in_lines_never_reach_the_stored_file() {
    let tmp = TempDir::new("secret");
    let mut up = connected(&tmp.0, "dev-1");
    let blob = "Q".repeat(48);
    let evil = json!({
        "ts": "2026-11-02T13:00:00.000Z", "level": "info", "src": "browser", "target": "t",
        "msg": format!("pairing_code=GRAPE-4821 Authorization: Bearer abc.def.ghi blob {blob} https://x/y?token=URLSECRET&a=1"),
        "fields": {"token": "FIELDSECRET", "nested": {"password": "hunter2hunter2"}}
    })
    .to_string();
    send(&mut up, "logs-batch", &batch("browser", 1, 0, 1, &[evil])).unwrap();
    assert_eq!(drain(&mut up, 1)[0]["status"], "ok");
    let stored = read_all(&tmp.0.join("clients"));
    for leak in [
        "GRAPE-4821",
        "abc.def.ghi",
        blob.as_str(),
        "URLSECRET",
        "FIELDSECRET",
        "hunter2hunter2",
    ] {
        assert!(!stored.contains(leak), "leaked {leak}: {stored}");
    }
    assert!(stored.contains("[redacted"));
}

#[test]
fn a_client_cannot_write_under_another_device_id() {
    let tmp = TempDir::new("spoof");
    let mut up = connected(&tmp.0, "attacker-1");
    // Every place a client can put an id: the line's device field, a device-looking stream, and a
    // report's device block. None of them chooses a directory or a file name.
    let spoofed = json!({"ts":"2026-11-02T13:00:00.000Z","level":"info","src":"app-win",
                         "device":"victim","pc":"victim","msg":"x"})
    .to_string();
    send(
        &mut up,
        "logs-batch",
        &batch("app-win", 1, 0, 1, &[spoofed]),
    )
    .unwrap();
    assert_eq!(drain(&mut up, 1)[0]["status"], "ok");
    let replies = send(&mut up, "logs-batch", &batch("victim", 2, 0, 1, &[])).unwrap();
    assert_eq!(replies[0]["status"], "rejected");
    let mut with_device = batch("app-win", 3, 1, 2, &[]);
    with_device["device"] = json!("victim");
    with_device["deviceId"] = json!("victim");
    send(&mut up, "logs-batch", &with_device).unwrap();
    assert_eq!(drain(&mut up, 1)[0]["status"], "ok");
    upload_report(&mut up, "11111111-2222-4333-8444-555555555555", &[]);
    assert_eq!(drain(&mut up, 1)[0]["status"], "ok");

    let clients: Vec<String> = files_in(&tmp.0.join("clients"), "");
    assert_eq!(clients, vec!["attacker-1".to_string()]);
    let line_text = fs::read_to_string(tmp.0.join("clients/attacker-1/2026-11-02.jsonl")).unwrap();
    let stored: Value = serde_json::from_str(line_text.trim()).unwrap();
    assert_eq!(stored["device"], "attacker-1");
    let feedback = files_in(&tmp.0.join("feedback"), ".json");
    assert_eq!(feedback.len(), 1);
    assert!(feedback[0].ends_with("-attacker-1.json"), "{feedback:?}");
    let doc: Value = serde_json::from_str(
        &fs::read_to_string(tmp.0.join("feedback").join(&feedback[0])).unwrap(),
    )
    .unwrap();
    assert_eq!(doc["received_from"], "attacker-1");
    assert!(!tmp.0.join("clients/victim").exists());
}

// ---------------------------------------------------------------- feedback

#[test]
fn feedback_assembles_chunks_and_acks_after_the_file_is_on_disk() {
    let tmp = TempDir::new("fb");
    let mut up = connected(&tmp.0, "dev-1");
    let id = "aaaaaaaa-0000-4000-8000-000000000001";
    let image = png(MAX_FEEDBACK_CHUNK_BYTES * 2 + 123);
    let immediate = upload_report(&mut up, id, &image);
    assert!(immediate.is_empty(), "no ack before the file is stored");
    let ack = drain(&mut up, 1);
    assert_eq!(ack[0], feedback_ack_message(id, FeedbackStatus::Ok, ""));
    let dir = tmp.0.join("feedback");
    let json_files = files_in(&dir, ".json");
    assert_eq!(json_files.len(), 1);
    let stem = json_files[0].trim_end_matches(".json");
    assert_eq!(fs::read(dir.join(format!("{stem}.png"))).unwrap(), image);
    let doc: Value =
        serde_json::from_str(&fs::read_to_string(dir.join(&json_files[0])).unwrap()).unwrap();
    assert_eq!(doc["text"], "Stream froze");
    assert_eq!(doc["screenshot"], format!("{stem}.png"));

    // No screenshot: begin alone completes the report.
    let id2 = "aaaaaaaa-0000-4000-8000-000000000002";
    assert!(upload_report(&mut up, id2, &[]).is_empty());
    assert_eq!(drain(&mut up, 1)[0]["status"], "ok");
    assert_eq!(files_in(&dir, ".json").len(), 2);
    assert_eq!(files_in(&dir, ".png").len(), 1);
}

#[test]
fn a_duplicate_report_id_is_acked_ok_without_storing_again() {
    let tmp = TempDir::new("fb-dup");
    let id = "bbbbbbbb-0000-4000-8000-000000000001";
    let image = png(100);
    let mut up = connected(&tmp.0, "dev-1");
    upload_report(&mut up, id, &image);
    assert_eq!(drain(&mut up, 1)[0]["status"], "ok");
    // Same connection: answered at once from memory, even before the chunks.
    let again = send(&mut up, "feedback-begin", &begin(id, image.len())).unwrap();
    assert_eq!(
        again,
        vec![feedback_ack_message(id, FeedbackStatus::Ok, "")]
    );
    // A late chunk of the stored report is acked ok too and changes nothing.
    let late = send(&mut up, "feedback-chunk", &chunk(id, 0, &image)).unwrap();
    assert_eq!(late[0]["status"], "ok");
    drop(up);
    // A new connection after a host restart: the store knows the id from disk.
    let mut up = connected(&tmp.0, "dev-1");
    assert!(upload_report(&mut up, id, &[]).is_empty());
    assert_eq!(drain(&mut up, 1)[0]["status"], "ok");
    // Now remembered in memory: answered at once.
    let again = upload_report(&mut up, id, &[]);
    assert_eq!(
        again,
        vec![feedback_ack_message(id, FeedbackStatus::Ok, "")]
    );
    assert_eq!(files_in(&tmp.0.join("feedback"), ".json").len(), 1);
    // The same id from another device is a different report.
    let mut other = connected(&tmp.0, "dev-2");
    upload_report(&mut other, id, &[]);
    assert_eq!(drain(&mut other, 1)[0]["status"], "ok");
    assert_eq!(files_in(&tmp.0.join("feedback"), ".json").len(), 2);
}

#[test]
fn a_stalled_report_times_out_after_60_seconds_and_the_next_one_proceeds() {
    let tmp = TempDir::new("fb-timeout");
    let mut up = connected(&tmp.0, "dev-1");
    let t0 = Instant::now();
    let a = "cccccccc-0000-4000-8000-00000000000a";
    let b = "cccccccc-0000-4000-8000-00000000000b";
    let image = png(MAX_FEEDBACK_CHUNK_BYTES + 10);
    up.on_message_at("feedback-begin", &begin(a, image.len()).to_string(), t0)
        .unwrap();
    up.on_message_at(
        "feedback-chunk",
        &chunk(a, 0, &image[..MAX_FEEDBACK_CHUNK_BYTES]).to_string(),
        t0 + Duration::from_secs(1),
    )
    .unwrap();
    // One report at a time: another begin 30 s later is told to retry.
    let busy = up
        .on_message_at(
            "feedback-begin",
            &begin(b, 0).to_string(),
            t0 + Duration::from_secs(31),
        )
        .unwrap();
    assert_eq!(busy[0]["status"], "retry");
    assert_eq!(busy[0]["id"], b);
    // 60 s after the last chunk the first report is abandoned with a retry ack.
    assert!(up.poll_at(t0 + Duration::from_secs(60)).is_empty());
    let expired = up.poll_at(t0 + Duration::from_secs(61));
    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0]["id"], a);
    assert_eq!(expired[0]["status"], "retry");
    assert!(files_in(&tmp.0.join("feedback"), ".json").is_empty());
    // The late chunk of the abandoned report is refused, the other report goes through.
    let stale = up
        .on_message_at(
            "feedback-chunk",
            &chunk(a, 1, &image[MAX_FEEDBACK_CHUNK_BYTES..]).to_string(),
            t0 + Duration::from_secs(62),
        )
        .unwrap();
    assert_eq!(stale[0]["status"], "rejected");
    up.on_message_at(
        "feedback-begin",
        &begin(b, 0).to_string(),
        t0 + Duration::from_secs(62),
    )
    .unwrap();
    assert_eq!(drain(&mut up, 1)[0]["status"], "ok");
    // An expired report is also replaced by a new begin without waiting for the poll.
    let c = "cccccccc-0000-4000-8000-00000000000c";
    let d = "cccccccc-0000-4000-8000-00000000000d";
    up.on_message_at("feedback-begin", &begin(c, image.len()).to_string(), t0)
        .unwrap();
    let replaced = up
        .on_message_at(
            "feedback-begin",
            &begin(d, 0).to_string(),
            t0 + Duration::from_secs(90),
        )
        .unwrap();
    assert!(replaced.is_empty());
    assert_eq!(drain(&mut up, 1)[0]["id"], d);
}

#[test]
fn feedback_size_and_shape_limits() {
    let tmp = TempDir::new("fb-limits");
    let mut up = connected(&tmp.0, "dev-1");
    let id = "dddddddd-0000-4000-8000-000000000001";
    let status = |replies: Vec<Value>| replies[0]["status"].as_str().unwrap().to_string();
    let reason = |replies: &Vec<Value>| replies[0]["reason"].as_str().unwrap().to_string();

    // Over 5 MiB, in the begin message.
    let mut big = begin(id, 0);
    big["pngBytes"] = json!(MAX_FEEDBACK_PNG_BYTES + 1);
    big["chunks"] = json!((MAX_FEEDBACK_PNG_BYTES + 1).div_ceil(MAX_FEEDBACK_CHUNK_BYTES));
    let r = send(&mut up, "feedback-begin", &big).unwrap();
    assert_eq!(status(r.clone()), "rejected");
    assert_eq!(reason(&r), "screenshot too large");
    // Exactly 5 MiB is accepted at begin.
    let mut max = begin(id, 0);
    max["pngBytes"] = json!(MAX_FEEDBACK_PNG_BYTES);
    max["chunks"] = json!(MAX_FEEDBACK_PNG_BYTES.div_ceil(MAX_FEEDBACK_CHUNK_BYTES));
    assert!(send(&mut up, "feedback-begin", &max).unwrap().is_empty());
    // Chunk count must match the size; pngBytes 0 means chunks 0.
    let mut bad_count = begin(id, 100);
    bad_count["chunks"] = json!(2);
    assert_eq!(
        status(send(&mut up, "feedback-begin", &bad_count).unwrap()),
        "rejected"
    );
    let mut zero = begin(id, 0);
    zero["chunks"] = json!(1);
    assert_eq!(
        status(send(&mut up, "feedback-begin", &zero).unwrap()),
        "rejected"
    );
    // The report id must match the message id.
    let mut mismatch = begin(id, 0);
    mismatch["report"]["id"] = json!("other-id");
    assert_eq!(
        status(send(&mut up, "feedback-begin", &mismatch).unwrap()),
        "rejected"
    );
    // Unusable ids are dropped.
    let mut bad_id = begin(id, 0);
    bad_id["id"] = json!("../../x");
    assert!(send(&mut up, "feedback-begin", &bad_id).is_err());

    // Chunk problems each end the report with a rejected ack.
    let start_fresh = |up: &mut LogUpload, len: usize| {
        // Replace whatever is open (begin of the same id restarts it).
        assert!(send(up, "feedback-begin", &begin(id, len))
            .unwrap()
            .is_empty());
    };
    start_fresh(&mut up, 100);
    let r = send(&mut up, "feedback-chunk", &chunk(id, 1, &png(100))).unwrap();
    assert_eq!(reason(&r), "chunk out of order");
    start_fresh(&mut up, 100);
    let r = send(&mut up, "feedback-chunk", &chunk(id, 0, &[1u8; 101])).unwrap();
    assert_eq!(reason(&r), "screenshot larger than announced");
    start_fresh(&mut up, 100);
    let bad = json!({"type":"feedback-chunk","v":1,"id":id,"i":0,"data":"***not base64***"});
    let r = send(&mut up, "feedback-chunk", &bad).unwrap();
    assert_eq!(reason(&r), "chunk is not valid base64");
    start_fresh(&mut up, MAX_FEEDBACK_CHUNK_BYTES + 1);
    let r = send(
        &mut up,
        "feedback-chunk",
        &chunk(id, 0, &vec![2u8; MAX_FEEDBACK_CHUNK_BYTES + 1]),
    )
    .unwrap();
    assert_eq!(reason(&r), "chunk too large");
    let huge = json!({"type":"feedback-chunk","v":1,"id":id,"i":0,"data":"A".repeat(200_000)});
    start_fresh(&mut up, MAX_FEEDBACK_CHUNK_BYTES + 1);
    assert_eq!(
        reason(&send(&mut up, "feedback-chunk", &huge).unwrap()),
        "chunk too large"
    );
    // The last chunk must complete the announced size.
    start_fresh(&mut up, MAX_FEEDBACK_CHUNK_BYTES + 10);
    send(&mut up, "feedback-chunk", &chunk(id, 0, &[3u8; 100])).unwrap();
    let r = send(&mut up, "feedback-chunk", &chunk(id, 1, &[3u8; 5])).unwrap();
    assert_eq!(reason(&r), "screenshot smaller than announced");
    // A chunk with no report in progress, and a chunk for another report.
    let r = send(&mut up, "feedback-chunk", &chunk(id, 0, &png(8))).unwrap();
    assert_eq!(reason(&r), "no report in progress");
    start_fresh(&mut up, 100);
    let other = send(
        &mut up,
        "feedback-chunk",
        &chunk("eeeeeeee-0", 0, &png(100)),
    )
    .unwrap();
    assert_eq!(reason(&other), "chunk belongs to another report");
    // Unpadded base64 is accepted, a PNG whose bytes arrive intact is stored.
    let image = png(100);
    let unpadded = json!({"type":"feedback-chunk","v":1,"id":id,"i":0,
                          "data": STANDARD_NO_PAD.encode(&image)});
    assert!(send(&mut up, "feedback-chunk", &unpadded)
        .unwrap()
        .is_empty());
    assert_eq!(drain(&mut up, 1)[0]["status"], "ok");
    assert_eq!(files_in(&tmp.0.join("feedback"), ".png").len(), 1);
}

#[test]
fn planted_secrets_in_a_report_never_reach_the_stored_file() {
    let tmp = TempDir::new("fb-secret");
    let mut up = connected(&tmp.0, "dev-1");
    let id = "ffffffff-0000-4000-8000-000000000001";
    let blob = "Z".repeat(60);
    let mut b = begin(id, 0);
    b["report"]["text"] = json!(format!(
        "password=hunter2hunter2 token: TEXTSECRET Bearer abc.def.ghi {blob}"
    ));
    b["report"]["logs"] = json!([{"ts":"2026-11-02T13:00:00.000Z","level":"info","msg":"x",
                                  "fields":{"token":"LOGSECRET"}}]);
    send(&mut up, "feedback-begin", &b).unwrap();
    assert_eq!(drain(&mut up, 1)[0]["status"], "ok");
    let stored = read_all(&tmp.0.join("feedback"));
    for leak in [
        "hunter2hunter2",
        "TEXTSECRET",
        "abc.def.ghi",
        blob.as_str(),
        "LOGSECRET",
    ] {
        assert!(!stored.contains(leak), "leaked {leak}: {stored}");
    }
}

// ---------------------------------------------------------------- connections

#[test]
fn answers_of_an_earlier_connection_are_discarded() {
    let tmp = TempDir::new("generation");
    let mut up = connected(&tmp.0, "dev-1");
    let pause = up.pause_handle();
    let guard = pause.lock().unwrap();
    let l = line("app-win", "2026-11-02T13:00:00.000Z", "x");
    send(&mut up, "logs-batch", &batch("app-win", 1, 0, 1, &[l])).unwrap();
    // The viewer reconnects (another device) while the worker still holds the job.
    up.start("dev-2", "Test phone", true);
    drop(guard);
    let got = drain(&mut up, 1);
    assert_eq!(got[0]["type"], "logs-state", "{got:?}");
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        up.poll().is_empty(),
        "the old viewer's ack is not delivered"
    );
    // The batch was still stored, under the device that sent it.
    assert!(tmp.0.join("clients/dev-1/2026-11-02.jsonl").is_file());
    assert!(files_in(&tmp.0.join("clients/dev-2"), ".jsonl").is_empty());
}

#[test]
fn unusable_device_ids_turn_the_upload_off() {
    let tmp = TempDir::new("badid");
    let mut up = upload(&tmp.0);
    for bad in ["", "../x", "a b", "nul"] {
        up.start(bad, "Test phone", true);
        assert!(!up.enabled(), "{bad:?}");
        let b = batch("app-win", 1, 0, 0, &[]);
        assert!(send(&mut up, "logs-batch", &b).is_err());
    }
}

// ---------------------------------------------------------------- retention and device.json

fn client_day_file(root: &Path, device: &str, day: &str) -> PathBuf {
    let dir = root.join("clients").join(device);
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{day}.jsonl"));
    fs::write(&path, "{}\n").unwrap();
    path
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "timed out: {what}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn sweep_runs_at_start_and_every_interval_and_leaves_feedback_alone() {
    let tmp = TempDir::new("sweep");
    // The fixed clock is 2026-11-02: 14 days back is 2026-10-19.
    let old = client_day_file(&tmp.0, "dev-1", "2026-10-18"); // 15 days
    let keep = client_day_file(&tmp.0, "dev-1", "2026-10-20"); // 13 days
    let today = client_day_file(&tmp.0, "dev-1", "2026-11-02");
    fs::create_dir_all(tmp.0.join("feedback")).unwrap();
    let report = tmp.0.join("feedback/20200101T000000000Z-dev-1.json");
    fs::write(&report, "{}").unwrap();
    let report_png = tmp.0.join("feedback/20200101T000000000Z-dev-1.png");
    fs::write(&report_png, "x").unwrap();

    let up = LogUpload::open_with(
        &tmp.0,
        UploadLimits {
            sweep_interval: Duration::from_millis(40),
            ..limits()
        },
    )
    .unwrap();
    wait_until("start-up sweep", || !old.exists());
    assert!(keep.exists() && today.exists());
    assert!(report.exists() && report_png.exists());

    // A file that ages out while the host runs goes at the next interval.
    let late = client_day_file(&tmp.0, "dev-2", "2026-10-01");
    wait_until("periodic sweep", || !late.exists());
    assert!(keep.exists() && today.exists());
    assert!(report.exists() && report_png.exists());
    drop(up);
}

#[test]
fn the_default_sweep_interval_is_24_hours() {
    assert_eq!(UploadLimits::default().sweep_interval, SWEEP_INTERVAL);
    assert_eq!(SWEEP_INTERVAL, Duration::from_secs(86_400));
}

fn device_json(root: &Path, device: &str) -> Value {
    serde_json::from_str(
        &fs::read_to_string(root.join("clients").join(device).join("device.json")).unwrap(),
    )
    .unwrap()
}

#[test]
fn device_json_is_written_on_the_first_batch_and_updated_on_reconnect() {
    let tmp = TempDir::new("devjson");
    let mut up = upload(&tmp.0);
    up.start("dev-1", "Pixel", true);
    drain(&mut up, 1);
    assert!(
        !tmp.0.join("clients/dev-1").exists(),
        "no folder before the first batch"
    );
    send(&mut up, "logs-batch", &message("logs-batch")).unwrap();
    drain(&mut up, 1);
    let meta = device_json(&tmp.0, "dev-1");
    assert_eq!(meta["device"], "dev-1");
    assert_eq!(meta["name"], "Pixel");
    assert_eq!(meta["platform"], "android");
    assert_eq!(meta["app_version"], "unknown");
    assert_eq!(meta["last_seen"], "2026-11-02T14:00:00.000Z");

    // The device was renamed and reconnects: the next first batch updates device.json.
    up.start("dev-1", "Pixel 8", true);
    drain(&mut up, 1);
    let second = batch(
        "app-win",
        2,
        0,
        1,
        &[line("app-win", "2026-11-02T13:00:00.000Z", "x")],
    );
    send(&mut up, "logs-batch", &second).unwrap();
    drain(&mut up, 1);
    let meta = device_json(&tmp.0, "dev-1");
    assert_eq!(meta["name"], "Pixel 8");
    assert_eq!(meta["platform"], "windows");
}

#[test]
fn an_unauthenticated_connection_never_creates_a_folder_or_device_json() {
    let tmp = TempDir::new("devjson-unauth");
    let mut up = upload(&tmp.0);
    send(&mut up, "logs-batch", &message("logs-batch")).unwrap_err();
    std::thread::sleep(Duration::from_millis(50));
    assert!(up.poll().is_empty());
    assert!(!tmp.0.join("clients").read_dir().unwrap().any(|_| true));
    // A device whose id is unusable has no connection either.
    up.start("../escape", "Evil", true);
    send(&mut up, "logs-batch", &message("logs-batch")).unwrap_err();
    assert!(!tmp.0.join("clients").read_dir().unwrap().any(|_| true));
}
