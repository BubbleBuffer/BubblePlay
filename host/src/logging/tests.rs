use super::*;
use std::sync::atomic::AtomicU32;

/// Scoped subscriber, serialized with the other tests that install one.
fn with_default<S, T>(subscriber: S, body: impl FnOnce() -> T) -> T
where
    S: tracing::Subscriber + Send + Sync + 'static,
{
    let _serial = crate::test_tracing::serial();
    tracing::subscriber::with_default(subscriber, body)
}
use tracing_subscriber::layer::SubscriberExt;

static COUNTER: AtomicU32 = AtomicU32::new(0);
/// The panic hook is process-wide; tests that replace it take turns.
static HOOK_LOCK: Mutex<()> = Mutex::new(());

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bubbleplay-logging-test-{name}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn read_all(dir: &Path) -> String {
    let mut names: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    names.sort();
    names
        .iter()
        .map(|p| fs::read_to_string(p).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("")
}

fn lines(dir: &Path) -> Vec<Value> {
    read_all(dir)
        .lines()
        .map(|l| serde_json::from_str(l).expect("every line is JSON"))
        .collect()
}

fn logger(name: &str, src: &'static str) -> (Logger, PathBuf) {
    let dir = temp_dir(name);
    (Logger::new(LogConfig::new(src, dir.clone())), dir)
}

#[test]
fn layer_writes_spec_conformant_lines() {
    let (logger, dir) = logger("layer", "host");
    logger.set_device(Some("dev-123"));
    let subscriber = tracing_subscriber::registry().with(logger.layer());
    with_default(subscriber, || {
        tracing::info!(pc = "pc-9", session = "s-1", count = 3u64, "hello world");
        tracing::warn!(target: "custom::target", error = %"boom", "something failed");
    });
    logger.shutdown();
    let lines = lines(&dir);
    assert_eq!(lines.len(), 2);
    let first = &lines[0];
    let ts = first["ts"].as_str().unwrap();
    assert_eq!(ts.len(), 24, "{ts}");
    assert!(ts.ends_with('Z') && ts.as_bytes()[10] == b'T' && ts.as_bytes()[19] == b'.');
    assert_eq!(first["level"], "info");
    assert_eq!(first["src"], "host");
    assert_eq!(first["device"], "dev-123");
    assert_eq!(first["pc"], "pc-9");
    assert_eq!(first["session"], "s-1");
    assert_eq!(first["msg"], "hello world");
    assert_eq!(first["fields"]["count"], 3);
    assert!(first["target"].as_str().unwrap().contains("logging"));
    assert_eq!(lines[1]["level"], "warn");
    assert_eq!(lines[1]["target"], "custom::target");
    assert_eq!(lines[1]["fields"]["error"], "boom");
    assert_eq!(lines[1]["pc"], Value::Null);
    // Key order starts with ts and level.
    let raw = read_all(&dir);
    assert!(raw.starts_with(r#"{"ts":""#), "{raw}");
    let name = fs::read_dir(&dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .file_name();
    let name = name.to_string_lossy().into_owned();
    assert!(
        name.starts_with("host-") && name.ends_with(".jsonl"),
        "{name}"
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn error_fields_carry_the_whole_chain() {
    let (logger, dir) = logger("chain", "host");
    let error = anyhow::anyhow!("socket reset")
        .context("ws read")
        .context("serve failed");
    let subscriber = tracing_subscriber::registry().with(logger.layer());
    with_default(subscriber, || {
        tracing::error!(error = %format!("{error:#}"), "serve session failed");
    });
    logger.shutdown();
    let lines = lines(&dir);
    assert_eq!(
        lines[0]["fields"]["error"],
        "serve failed: ws read: socket reset"
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn registered_literals_and_secret_fields_never_reach_the_file() {
    let literal = "literal-secret-value-7781";
    redact::register_secret(literal);
    let (logger, dir) = logger("secrets", "tray");
    let subscriber = tracing_subscriber::registry().with(logger.layer());
    let blob = "Z".repeat(60);
    with_default(subscriber, || {
        tracing::info!("connecting with {literal} now");
        tracing::info!(password = "hunter2hunter2", token = %literal, note = literal, "fields");
        tracing::info!("url wss://relay/ws?token=abcdef123456&room=r1 Bearer abc.def");
        tracing::info!(blob = %blob, "pairing code: 654321");
    });
    logger.shutdown();
    let raw = read_all(&dir);
    for leaked in [
        literal,
        "hunter2hunter2",
        "abcdef123456",
        "abc.def",
        "654321",
        blob.as_str(),
    ] {
        assert!(!raw.contains(leaked), "{leaked} leaked: {raw}");
    }
    assert!(raw.contains("room=r1"));
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn panic_hook_writes_an_error_line_with_a_stack() {
    let _guard = HOOK_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (logger, dir) = logger("panic", "agent");
    install_panic_hook(&logger);
    let result = std::thread::Builder::new()
        .name("panicker".into())
        .spawn(|| {
            let value: Option<u32> = std::hint::black_box(None);
            value.expect("the thing was missing");
        })
        .unwrap()
        .join();
    assert!(result.is_err());
    // Restore the default hook for the rest of the test run.
    let _ = std::panic::take_hook();
    // No shutdown or explicit flush: the hook flushed before the thread died.
    let lines = lines(&dir);
    let line = lines
        .iter()
        .find(|l| l["target"] == "panic")
        .expect("a panic line was written");
    assert_eq!(line["level"], "error");
    assert_eq!(line["src"], "agent");
    assert!(line["msg"]
        .as_str()
        .unwrap()
        .contains("the thing was missing"));
    assert_eq!(line["fields"]["thread"], "panicker");
    assert!(line["fields"]["stack"]
        .as_str()
        .is_some_and(|s| !s.is_empty()));
    assert!(line["fields"]["stack"].as_str().unwrap().len() <= MAX_STACK_BYTES);
    logger.shutdown();
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn panic_message_is_redacted() {
    let (logger, dir) = logger("panic-redact", "host");
    logger.log_panic(
        "bad token=abc12345 value",
        Some("x.rs:1:1"),
        "main",
        "stack here",
    );
    let raw = read_all(&dir);
    assert!(!raw.contains("abc12345"), "{raw}");
    assert!(raw.contains(r#""target":"panic""#));
    logger.shutdown();
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn long_lines_are_truncated_to_the_cap() {
    let (logger, dir) = logger("truncate", "host");
    let mut fields = Map::new();
    fields.insert("stack".into(), Value::String("s ".repeat(10_000)));
    fields.insert("note".into(), Value::String("n ".repeat(10_000)));
    logger.log(Level::INFO, "t", &"m ".repeat(10_000), fields);
    let mut quote = Map::new();
    quote.insert("x".into(), json!(1));
    logger.log(Level::INFO, "t", &"\"q\n".repeat(5_000), quote);
    logger.log(Level::INFO, "t", &"é".repeat(9_000), Map::new());
    logger.shutdown();
    let raw = read_all(&dir);
    let all: Vec<&str> = raw.lines().collect();
    assert_eq!(all.len(), 3);
    for line in &all {
        assert!(line.len() <= MAX_LINE_BYTES, "{}", line.len());
        let parsed: Value = serde_json::from_str(line).unwrap();
        assert!(
            parsed["msg"].as_str().unwrap().ends_with(TRUNCATED_MARK)
                || parsed["fields"].is_object()
        );
    }
    assert!(serde_json::from_str::<Value>(all[0]).unwrap()["msg"]
        .as_str()
        .unwrap()
        .ends_with(TRUNCATED_MARK));
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn retention_deletes_old_day_files_and_enforces_the_count_cap() {
    let dir = temp_dir("retention");
    let today = days_from_civil(2026, 11, 10);
    for (y, m, d, index) in [
        (2026, 11, 10, 0),
        (2026, 11, 3, 0), // exactly 7 days old: kept
        (2026, 11, 2, 0), // 8 days old: deleted
        (2026, 11, 2, 1), // deleted
        (2026, 10, 1, 0), // deleted
    ] {
        fs::write(
            dir.join(day_file_name("host", days_from_civil(y, m, d), index)),
            "{}\n",
        )
        .unwrap();
    }
    // Other sources and unrelated files are left alone.
    fs::write(dir.join("tray-2026-01-01.jsonl"), "x").unwrap();
    fs::write(dir.join("host-notes.txt"), "x").unwrap();
    fs::write(dir.join("host-2026-13-01.jsonl"), "x").unwrap();
    let deleted = apply_retention(&dir, "host", today, 7, 20, None);
    assert_eq!(deleted, 3);
    let mut left: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    assert_eq!(
        left,
        vec![
            "host-2026-11-03.jsonl",
            "host-2026-11-10.jsonl",
            "host-2026-13-01.jsonl",
            "host-notes.txt",
            "tray-2026-01-01.jsonl"
        ]
    );
    // Count cap: keep the newest N.
    for day in 4..10 {
        fs::write(
            dir.join(day_file_name("host", days_from_civil(2026, 11, day), 0)),
            "{}\n",
        )
        .unwrap();
    }
    let deleted = apply_retention(&dir, "host", today, 7, 3, None);
    assert_eq!(deleted, 5);
    assert!(dir.join("host-2026-11-10.jsonl").exists());
    assert!(dir.join("host-2026-11-09.jsonl").exists());
    assert!(dir.join("host-2026-11-08.jsonl").exists());
    assert!(!dir.join("host-2026-11-07.jsonl").exists());
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn writer_rotates_by_day_and_applies_retention() {
    let (logger, dir) = logger("rotate", "helper");
    let day = |y, m, d| {
        UNIX_EPOCH + Duration::from_secs(days_from_civil(y, m, d) as u64 * 86_400 + 12 * 3600)
    };
    // An old file from a previous run, outside retention.
    fs::write(
        dir.join(day_file_name("helper", days_from_civil(2026, 10, 1), 0)),
        "{}\n",
    )
    .unwrap();
    logger.log_at(day(2026, 11, 2), Level::INFO, "t", "first day", Map::new());
    logger.log_at(day(2026, 11, 3), Level::INFO, "t", "second day", Map::new());
    logger.shutdown();
    let mut names: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec!["helper-2026-11-02.jsonl", "helper-2026-11-03.jsonl"]
    );
    let second = fs::read_to_string(dir.join("helper-2026-11-03.jsonl")).unwrap();
    assert!(second.contains("second day") && !second.contains("first day"));
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn day_files_roll_over_at_the_size_cap() {
    let dir = temp_dir("size");
    let mut config = LogConfig::new("host", dir.clone());
    config.max_file_bytes = 1024;
    let logger = Logger::new(config);
    for i in 0..20 {
        logger.log(
            Level::INFO,
            "t",
            &format!("line {i} {}", "x".repeat(200)),
            Map::new(),
        );
    }
    logger.shutdown();
    let files: Vec<_> = fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).collect();
    assert!(files.len() > 1, "{} files", files.len());
    for file in files {
        assert!(file.metadata().unwrap().len() <= 1024 + 300);
    }
    assert_eq!(lines(&dir).len(), 20);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn full_queue_drops_oldest_and_counts_without_blocking() {
    let dir = temp_dir("drop");
    let mut config = LogConfig::new("host", dir.clone());
    config.queue_capacity = 4;
    let logger = Logger::new(config);
    let started = Instant::now();
    for i in 0..20_000 {
        logger.log(Level::INFO, "t", &format!("n{i}"), Map::new());
    }
    assert!(started.elapsed() < Duration::from_secs(30));
    let dropped = logger.pending_dropped();
    logger.shutdown();
    let written = lines(&dir).len() as u64;
    assert_eq!(written + dropped, 20_000);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn timestamps_are_utc_rfc3339_with_milliseconds() {
    let (ts, days) = timestamp(UNIX_EPOCH + Duration::from_millis(1_793_626_512_345));
    assert_eq!(ts, "2026-11-02T13:35:12.345Z");
    assert_eq!(days, days_from_civil(2026, 11, 2));
    assert_eq!(civil_from_days(days), (2026, 11, 2));
    assert_eq!(timestamp(UNIX_EPOCH).0, "1970-01-01T00:00:00.000Z");
}

#[test]
fn day_file_names_parse_strictly() {
    assert_eq!(
        parse_day_file("host", "host-2026-11-02.3.jsonl"),
        Some((days_from_civil(2026, 11, 2), 3))
    );
    assert_eq!(parse_day_file("host", "tray-2026-11-02.jsonl"), None);
    assert_eq!(parse_day_file("host", "host-2026-11-02.log"), None);
}

#[test]
fn active_session_fills_lines_without_their_own() {
    let (logger, dir) = logger("session", "host");
    let subscriber = tracing_subscriber::registry().with(logger.layer());
    with_default(subscriber, || {
        tracing::info!("before");
        logger.set_session(Some("0123456789abcdef0123456789abcdef"));
        tracing::info!("during");
        tracing::info!(session = "own", "explicit");
        logger.set_session(None);
        tracing::info!("after");
    });
    logger.shutdown();
    let lines = lines(&dir);
    assert_eq!(lines[0]["session"], Value::Null);
    assert_eq!(lines[1]["session"], "0123456789abcdef0123456789abcdef");
    assert_eq!(lines[2]["session"], "own");
    assert_eq!(lines[3]["session"], Value::Null);
    let _ = fs::remove_dir_all(dir);
}
