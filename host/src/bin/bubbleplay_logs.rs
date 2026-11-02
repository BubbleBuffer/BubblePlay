//! `bubbleplay-logs`: find log lines around a timestamp and list feedback reports.
//!
//! ```text
//! bubbleplay-logs around <UTC-RFC3339-ts> [--window 5m] [--dir <logs dir>] [--src host,tray,...] [--device <id>]
//! bubbleplay-logs feedback [--dir <logs dir>]
//! ```
//!
//! `around` merges the lines of every source (own `<src>-YYYY-MM-DD[.N].jsonl` files in the logs dir and
//! `clients/<device-id>/YYYY-MM-DD.jsonl`) that fall within the window on either side of the timestamp,
//! sorted by `ts`, each prefixed with its source file. `--src` filters on the line's `src` field; `--device`
//! limits the `clients/` part to one device. Plain text output.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use bubbleplay_host::client_logs::{format_millis, parse_day, parse_rfc3339_millis};
use bubbleplay_host::logging::default_log_dir;
use serde_json::Value;

const DAY_MS: i64 = 86_400_000;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    ts_ms: i64,
    source: String,
    line: String,
}

/// `5m`, `30s`, `2h`, `1d`, `250ms`; a bare number is seconds.
fn parse_window(text: &str) -> Option<i64> {
    let text = text.trim();
    let (digits, unit): (&str, i64) = if let Some(d) = text.strip_suffix("ms") {
        (d, 1)
    } else if let Some(d) = text.strip_suffix('s') {
        (d, 1000)
    } else if let Some(d) = text.strip_suffix('m') {
        (d, 60_000)
    } else if let Some(d) = text.strip_suffix('h') {
        (d, 3_600_000)
    } else if let Some(d) = text.strip_suffix('d') {
        (d, DAY_MS)
    } else {
        (text, 1000)
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<i64>().ok()?.checked_mul(unit)
}

/// First `YYYY-MM-DD` found in a file name, as days since the epoch.
fn file_day(name: &str) -> Option<i64> {
    (0..name.len().saturating_sub(9))
        .filter(|&i| name.is_char_boundary(i) && name.is_char_boundary(i + 10))
        .find_map(|i| parse_day(&name[i..i + 10]))
}

/// Whether a file with this name can hold lines inside [start_ms, end_ms]. Unknown names are read.
fn file_in_range(name: &str, start_ms: i64, end_ms: i64) -> bool {
    match file_day(name) {
        Some(day) => day >= start_ms.div_euclid(DAY_MS) && day <= end_ms.div_euclid(DAY_MS),
        None => true,
    }
}

/// Parse one JSONL line into an entry when its `ts` is in the window and `--src` allows it.
fn parse_entry(
    source: &str,
    line: &str,
    start_ms: i64,
    end_ms: i64,
    srcs: &[String],
) -> Option<Entry> {
    let line = line.trim_end_matches(['\r', '\n']);
    let value: Value = serde_json::from_str(line).ok()?;
    let ts_ms = parse_rfc3339_millis(value.get("ts")?.as_str()?)?;
    if ts_ms < start_ms || ts_ms > end_ms {
        return None;
    }
    if !srcs.is_empty() {
        let src = value.get("src").and_then(Value::as_str).unwrap_or("");
        if !srcs.iter().any(|s| s == src) {
            return None;
        }
    }
    Some(Entry {
        ts_ms,
        source: source.to_string(),
        line: line.to_string(),
    })
}

/// Sort by timestamp, then source; equal keys keep their original order (stable sort).
fn sort_entries(entries: &mut [Entry]) {
    entries.sort_by(|a, b| a.ts_ms.cmp(&b.ts_ms).then_with(|| a.source.cmp(&b.source)));
}

fn format_entry(entry: &Entry) -> String {
    format!("{}: {}", entry.source, entry.line)
}

/// `20261102T133832345Z-phone-1.json` -> (RFC3339 ts, device id).
fn parse_feedback_name(name: &str) -> Option<(String, String)> {
    let base = name.strip_suffix(".json")?;
    let (ts, device) = base.split_once('-')?;
    let b = ts.as_bytes();
    if b.len() != 19 || b[8] != b'T' || b[18] != b'Z' || device.is_empty() {
        return None;
    }
    let digits_ok = ts[..8].bytes().all(|c| c.is_ascii_digit())
        && ts[9..18].bytes().all(|c| c.is_ascii_digit());
    if !digits_ok {
        return None;
    }
    let rfc = format!(
        "{}-{}-{}T{}:{}:{}.{}Z",
        &ts[0..4],
        &ts[4..6],
        &ts[6..8],
        &ts[9..11],
        &ts[11..13],
        &ts[13..15],
        &ts[15..18]
    );
    parse_rfc3339_millis(&rfc)?;
    Some((rfc, device.to_string()))
}

fn first_line(text: &str) -> String {
    text.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_string()
}

fn jsonl_files(
    dir: &Path,
    label_prefix: &str,
    start_ms: i64,
    end_ms: i64,
) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".jsonl")
                && entry.path().is_file()
                && file_in_range(&name, start_ms, end_ms)
            {
                out.push((format!("{label_prefix}{name}"), entry.path()));
            }
        }
    }
    out.sort();
    out
}

fn collect(
    dir: &Path,
    center: i64,
    window: i64,
    srcs: &[String],
    device: Option<&str>,
) -> Vec<Entry> {
    let (start, end) = (center.saturating_sub(window), center.saturating_add(window));
    let mut files = jsonl_files(dir, "", start, end);
    let clients = dir.join("clients");
    if let Ok(entries) = fs::read_dir(&clients) {
        let mut ids: Vec<String> = entries
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|id| device.is_none_or(|want| want == id))
            .collect();
        ids.sort();
        for id in ids {
            files.extend(jsonl_files(
                &clients.join(&id),
                &format!("clients/{id}/"),
                start,
                end,
            ));
        }
    }
    let mut entries = Vec::new();
    for (label, path) in files {
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        entries.extend(
            text.lines()
                .filter_map(|l| parse_entry(&label, l, start, end, srcs)),
        );
    }
    sort_entries(&mut entries);
    entries
}

fn list_feedback(dir: &Path) -> Vec<String> {
    let mut rows: Vec<(String, String)> = Vec::new();
    if let Ok(entries) = fs::read_dir(dir.join("feedback")) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some((ts, device)) = parse_feedback_name(&name) else {
                continue;
            };
            let text = fs::read_to_string(entry.path())
                .ok()
                .and_then(|t| serde_json::from_str::<Value>(&t).ok())
                .and_then(|v| v.get("text").and_then(Value::as_str).map(first_line))
                .unwrap_or_default();
            rows.push((name.clone(), format!("{ts}  {device}  {name}  {text}")));
        }
    }
    rows.sort();
    rows.into_iter().map(|(_, row)| row).collect()
}

const USAGE: &str = "usage:\n  bubbleplay-logs around <UTC-RFC3339-ts> [--window 5m] [--dir <logs dir>] [--src host,tray,...] [--device <id>]\n  bubbleplay-logs feedback [--dir <logs dir>]";

fn run(args: &[String]) -> Result<(), String> {
    let mut it = args.iter();
    let cmd = it.next().ok_or(USAGE)?.as_str();
    let mut positional: Vec<&String> = Vec::new();
    let mut window = "5m".to_string();
    let mut dir: Option<PathBuf> = None;
    let mut srcs: Vec<String> = Vec::new();
    let mut device: Option<String> = None;
    while let Some(arg) = it.next() {
        let mut value = |name: &str| {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match arg.as_str() {
            "--window" => window = value("--window")?,
            "--dir" => dir = Some(PathBuf::from(value("--dir")?)),
            "--src" => {
                srcs = value("--src")?
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
            "--device" => device = Some(value("--device")?),
            "-h" | "--help" => return Err(USAGE.to_string()),
            other if other.starts_with("--") => {
                return Err(format!("unknown option {other}\n{USAGE}"));
            }
            _ => positional.push(arg),
        }
    }
    let dir = dir.unwrap_or_else(default_log_dir);
    match cmd {
        "around" => {
            let ts = positional.first().ok_or(USAGE)?;
            let center = parse_rfc3339_millis(ts)
                .ok_or_else(|| format!("not an RFC3339 timestamp: {ts}"))?;
            let window_ms =
                parse_window(&window).ok_or_else(|| format!("bad --window: {window}"))?;
            let entries = collect(&dir, center, window_ms, &srcs, device.as_deref());
            println!(
                "# {} lines within {window} of {} in {}",
                entries.len(),
                format_millis(center),
                dir.display()
            );
            for entry in &entries {
                println!("{}", format_entry(entry));
            }
            Ok(())
        }
        "feedback" => {
            for row in list_feedback(&dir) {
                println!("{row}");
            }
            Ok(())
        }
        _ => Err(USAGE.to_string()),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("{msg}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn l(ts: &str, src: &str, msg: &str) -> String {
        format!(r#"{{"ts":"{ts}","level":"info","src":"{src}","msg":"{msg}"}}"#)
    }

    #[test]
    fn window_parsing() {
        assert_eq!(parse_window("5m"), Some(300_000));
        assert_eq!(parse_window("30s"), Some(30_000));
        assert_eq!(parse_window("30"), Some(30_000));
        assert_eq!(parse_window("2h"), Some(7_200_000));
        assert_eq!(parse_window("1d"), Some(DAY_MS));
        assert_eq!(parse_window("250ms"), Some(250));
        assert_eq!(parse_window(""), None);
        assert_eq!(parse_window("m"), None);
        assert_eq!(parse_window("-5m"), None);
        assert_eq!(parse_window("5x"), None);
    }

    #[test]
    fn window_bounds_are_inclusive_and_filter_lines() {
        let center = parse_rfc3339_millis("2026-11-02T13:00:00Z").unwrap();
        let (start, end) = (center - 60_000, center + 60_000);
        let at = |ts: &str| parse_entry("f", &l(ts, "host", "m"), start, end, &[]);
        assert!(at("2026-11-02T12:59:00.000Z").is_some());
        assert!(at("2026-11-02T13:01:00.000Z").is_some());
        assert!(at("2026-11-02T12:58:59.999Z").is_none());
        assert!(at("2026-11-02T13:01:00.001Z").is_none());
        assert!(parse_entry("f", "garbage", start, end, &[]).is_none());
        assert!(parse_entry("f", r#"{"msg":"no ts"}"#, start, end, &[]).is_none());
        let srcs = vec!["tray".to_string()];
        let host = l("2026-11-02T13:00:00.000Z", "host", "m");
        let tray = l("2026-11-02T13:00:00.000Z", "tray", "m");
        assert!(parse_entry("f", &host, start, end, &srcs).is_none());
        assert!(parse_entry("f", &tray, start, end, &srcs).is_some());
    }

    #[test]
    fn merge_sorts_by_ts_then_source_and_prefixes_source() {
        let (start, end) = (0, i64::MAX / 2);
        let inputs = [
            (
                "host-2026-11-02.jsonl",
                l("2026-11-02T13:00:02.000Z", "host", "c"),
            ),
            (
                "clients/p/2026-11-02.jsonl",
                l("2026-11-02T13:00:01.000Z", "app-win", "b"),
            ),
            (
                "tray-2026-11-02.jsonl",
                l("2026-11-02T13:00:01.000Z", "tray", "b2"),
            ),
            (
                "host-2026-11-02.jsonl",
                l("2026-11-02T13:00:00.500Z", "host", "a"),
            ),
        ];
        let mut entries: Vec<Entry> = inputs
            .iter()
            .filter_map(|(s, line)| parse_entry(s, line, start, end, &[]))
            .collect();
        sort_entries(&mut entries);
        let msgs: Vec<&str> = entries
            .iter()
            .map(|e| {
                e.line
                    .split("\"msg\":\"")
                    .nth(1)
                    .unwrap()
                    .trim_end_matches("\"}")
            })
            .collect();
        assert_eq!(msgs, ["a", "b", "b2", "c"]);
        assert!(format_entry(&entries[1]).starts_with("clients/p/2026-11-02.jsonl: {"));
    }

    #[test]
    fn day_pruning_of_file_names() {
        let s = parse_rfc3339_millis("2026-11-02T23:59:00Z").unwrap();
        let e = parse_rfc3339_millis("2026-11-03T00:05:00Z").unwrap();
        assert!(file_in_range("host-2026-11-02.jsonl", s, e));
        assert!(file_in_range("host-2026-11-03.2.jsonl", s, e));
        assert!(file_in_range("2026-11-03.jsonl", s, e));
        assert!(!file_in_range("2026-11-01.jsonl", s, e));
        assert!(!file_in_range("host-2026-11-04.jsonl", s, e));
        assert!(file_in_range("odd-name.jsonl", s, e));
    }

    #[test]
    fn feedback_names_and_first_line() {
        let (ts, dev) = parse_feedback_name("20261102T133832345Z-phone-1.json").unwrap();
        assert_eq!(ts, "2026-11-02T13:38:32.345Z");
        assert_eq!(dev, "phone-1");
        assert!(parse_feedback_name("20261102T133832345Z-phone-1.png").is_none());
        assert!(parse_feedback_name("notes.json").is_none());
        assert!(parse_feedback_name("20261302T133832345Z-x.json").is_none());
        assert_eq!(first_line("\n  first \nsecond"), "first");
        assert_eq!(first_line(""), "");
    }

    #[test]
    fn collect_reads_own_and_client_files() {
        let dir = std::env::temp_dir().join(format!("bp-logs-cli-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("clients/p1")).unwrap();
        fs::create_dir_all(dir.join("clients/p2")).unwrap();
        let host = format!(
            "{}\n{}\n",
            l("2026-11-02T13:00:03.000Z", "host", "h1"),
            l("2026-11-02T09:00:00.000Z", "host", "far")
        );
        fs::write(dir.join("host-2026-11-02.jsonl"), host).unwrap();
        let c1 = format!("{}\n", l("2026-11-02T13:00:01.000Z", "app-win", "c1"));
        let c2 = format!("{}\n", l("2026-11-02T13:00:02.000Z", "app-win", "c2"));
        let wrong = format!(
            "{}\n",
            l("2026-11-02T13:00:00.000Z", "app-win", "wrong-day-file")
        );
        fs::write(dir.join("clients/p1/2026-11-02.jsonl"), c1).unwrap();
        fs::write(dir.join("clients/p2/2026-11-02.jsonl"), c2).unwrap();
        fs::write(dir.join("clients/p1/2026-10-01.jsonl"), wrong).unwrap();
        let center = parse_rfc3339_millis("2026-11-02T13:00:00Z").unwrap();
        let all = collect(&dir, center, 300_000, &[], None);
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].source, "clients/p1/2026-11-02.jsonl");
        assert_eq!(all[2].source, "host-2026-11-02.jsonl");
        let one = collect(&dir, center, 300_000, &[], Some("p2"));
        assert_eq!(one.len(), 2);
        let only_host = collect(&dir, center, 300_000, &["host".to_string()], None);
        assert_eq!(only_host.len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }
}
