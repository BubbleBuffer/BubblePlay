//! Host-side store for log batches uploaded by paired clients (spec 3, charter item 4).
//!
//! Layout under the logs root: `clients/<device-id>/YYYY-MM-DD.jsonl`, `device.json` and `cursor.json`.
//! The host never trusts the client: every line is parsed, validated, stamped with the authenticated
//! device id and re-redacted before it is written. Callers must hold an [`AuthenticatedDevice`], which only
//! an authenticated session should construct; rejecting unauthenticated peers stays the caller's duty,
//! the type just makes it explicit.
//!
//! Limits (see [`Limits`]): batch 64 KiB, line 8 KiB, 20 MiB per device, 200 MiB total, 8 MiB per device per
//! UTC day (rate cap, counted in memory per host process), 14 day retention ([`ClientLogStore::sweep`]).

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::logging::{self, redact};

pub const MAX_BATCH_BYTES: usize = 64 * 1024;
pub const MAX_LINE_BYTES: usize = logging::MAX_LINE_BYTES;
pub const MAX_DEVICE_ID_LEN: usize = 64;
pub const PER_DEVICE_CAP_BYTES: u64 = 20 * 1024 * 1024;
pub const TOTAL_CAP_BYTES: u64 = 200 * 1024 * 1024;
pub const DAILY_RATE_CAP_BYTES: u64 = 8 * 1024 * 1024;
pub const RETENTION_DAYS: i64 = 14;
/// Lines stamped further than this ahead of the host clock are dropped.
pub const FUTURE_SKEW_MS: i64 = 5 * 60 * 1000;
const META_FIELD_MAX: usize = 256;

const LEVELS: [&str; 5] = ["trace", "debug", "info", "warn", "error"];
const SOURCES: [&str; 9] = [
    "host",
    "tray",
    "agent",
    "helper",
    "relay",
    "app-win",
    "app-android",
    "app-android-core",
    "browser",
];
const RESERVED_NAMES: [&str; 22] = [
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngestError {
    InvalidDeviceId,
    BatchTooLarge {
        bytes: usize,
    },
    InvalidCursor,
    /// The device exceeded its daily byte budget; nothing was written and the cursor did not move.
    RateLimited,
    Io(String),
}

impl std::fmt::Display for IngestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IngestError::InvalidDeviceId => write!(f, "invalid device id"),
            IngestError::BatchTooLarge { bytes } => write!(f, "batch too large ({bytes} bytes)"),
            IngestError::InvalidCursor => write!(f, "invalid cursor"),
            IngestError::RateLimited => write!(f, "daily log upload limit reached"),
            IngestError::Io(msg) => write!(f, "io error: {msg}"),
        }
    }
}

impl std::error::Error for IngestError {}

impl From<std::io::Error> for IngestError {
    fn from(err: std::io::Error) -> Self {
        IngestError::Io(err.to_string())
    }
}

/// Check a device id: only `[A-Za-z0-9_-]`, 1..=64 chars, no Windows reserved device names.
pub fn sanitize_device_id(id: &str) -> Result<&str, IngestError> {
    let ok = !id.is_empty()
        && id.len() <= MAX_DEVICE_ID_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        && !RESERVED_NAMES.contains(&id.to_ascii_lowercase().as_str());
    if ok {
        Ok(id)
    } else {
        Err(IngestError::InvalidDeviceId)
    }
}

/// Proof that the holder authenticated as a paired device. Construct it only from the authenticated
/// session code (after the pairing/auth handshake succeeded); the id is validated here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthenticatedDevice {
    id: String,
}

impl AuthenticatedDevice {
    /// Only call this for a peer that completed authentication as a paired device.
    pub fn new(device_id: &str) -> Result<Self, IngestError> {
        sanitize_device_id(device_id)?;
        Ok(Self {
            id: device_id.to_string(),
        })
    }

    /// The PC's own pseudo device (reports and logs made on the PC itself, id `this-pc`).
    pub fn this_pc() -> Self {
        Self {
            id: "this-pc".to_string(),
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }
}

#[derive(Debug, Clone)]
pub struct Batch {
    /// Raw JSONL lines as the client sent them (without trailing newline).
    pub lines: Vec<String>,
    /// Client cursor before the first line of this batch.
    pub cursor_from: u64,
    /// Client cursor after the last line of this batch; this is what the host acks.
    pub cursor_to: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ack {
    /// Highest cursor the host has persisted; the client resumes from here.
    pub cursor: u64,
    pub written: usize,
    pub dropped_invalid: usize,
    pub dropped_oversize: usize,
    pub dropped_future: usize,
    pub dropped_old: usize,
    /// The whole batch was at or below the stored cursor and was not written again.
    pub duplicate: bool,
}

#[derive(Debug, Clone)]
pub struct DeviceMeta {
    pub name: String,
    pub platform: String,
    pub app_version: String,
}

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub per_device_bytes: u64,
    pub total_bytes: u64,
    pub daily_bytes: u64,
    pub retention_days: i64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            per_device_bytes: PER_DEVICE_CAP_BYTES,
            total_bytes: TOTAL_CAP_BYTES,
            daily_bytes: DAILY_RATE_CAP_BYTES,
            retention_days: RETENTION_DAYS,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepReport {
    pub removed_files: usize,
    pub removed_bytes: u64,
}

struct State {
    /// device -> (epoch day of the host clock, bytes accepted that day)
    daily: HashMap<String, (i64, u64)>,
}

pub struct ClientLogStore {
    clients: PathBuf,
    limits: Limits,
    state: Mutex<State>,
}

// ---------------------------------------------------------------- time helpers (shared with the CLI)

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = i64::from(if m > 2 { m - 3 } else { m + 9 });
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
                29
            } else {
                28
            }
        }
    }
}

fn num(s: &str) -> Option<i64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// Days since the Unix epoch for `YYYY-MM-DD`.
pub fn parse_day(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let y = num(&s[0..4])?;
    let m = num(&s[5..7])? as u32;
    let d = num(&s[8..10])? as u32;
    if !(1..=12).contains(&m) || d == 0 || d > days_in_month(y, m) {
        return None;
    }
    Some(days_from_civil(y, m, d))
}

/// Lenient RFC3339 to Unix milliseconds: `Z` or `+hh:mm`/`-hh:mm`, optional fraction (extra digits ignored).
pub fn parse_rfc3339_millis(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.len() < 20 || !s.is_ascii() {
        return None;
    }
    let b = s.as_bytes();
    if !(b[10] == b'T' || b[10] == b't') || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let days = parse_day(&s[0..10])?;
    let h = num(&s[11..13])?;
    let mi = num(&s[14..16])?;
    let sec = num(&s[17..19])?;
    if h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    let mut rest = &s[19..];
    let mut millis = 0i64;
    if let Some(frac) = rest.strip_prefix('.') {
        let digits = frac.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        let mut padded = frac[..digits.min(3)].to_string();
        while padded.len() < 3 {
            padded.push('0');
        }
        millis = num(&padded)?;
        rest = &frac[digits..];
    }
    let offset_secs = if rest == "Z" || rest == "z" {
        0
    } else {
        let sign = match rest.as_bytes().first()? {
            b'+' => 1,
            b'-' => -1,
            _ => return None,
        };
        let tz = &rest[1..];
        if tz.len() != 5 || tz.as_bytes()[2] != b':' {
            return None;
        }
        let oh = num(&tz[0..2])?;
        let om = num(&tz[3..5])?;
        if oh > 23 || om > 59 {
            return None;
        }
        sign * (oh * 3600 + om * 60)
    };
    Some(((days * 86_400 + h * 3600 + mi * 60 + sec - offset_secs) * 1000) + millis)
}

/// The spec's exact form: `YYYY-MM-DDTHH:MM:SS.mmmZ` (24 chars).
pub fn parse_ts_strict(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 24 || b[19] != b'.' || b[23] != b'Z' || b[10] != b'T' {
        return None;
    }
    parse_rfc3339_millis(s)
}

/// UTC RFC3339 with milliseconds for Unix milliseconds.
pub fn format_millis(ms: i64) -> String {
    let at = if ms >= 0 {
        UNIX_EPOCH + Duration::from_millis(ms as u64)
    } else {
        UNIX_EPOCH
    };
    logging::timestamp(at).0
}

fn epoch_ms(at: SystemTime) -> i64 {
    at.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------- file helpers

fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        file.write_all(data)?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn day_file_day(name: &str) -> Option<i64> {
    parse_day(name.strip_suffix(".jsonl")?)
}

/// (day, path, size) of the `YYYY-MM-DD.jsonl` files in a device directory.
fn day_files(dir: &Path) -> Vec<(i64, PathBuf, u64)> {
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let (Some(day), Ok(meta)) = (day_file_day(&name), entry.metadata()) {
                if meta.is_file() {
                    out.push((day, entry.path(), meta.len()));
                }
            }
        }
    }
    out
}

fn device_dirs(clients: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(clients) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if sanitize_device_id(&name).is_ok() && entry.path().is_dir() {
                out.push((name, entry.path()));
            }
        }
    }
    out
}

fn clamp_meta(text: &str) -> String {
    let redacted = redact::redact_text(text);
    let mut end = redacted.len().min(META_FIELD_MAX);
    while !redacted.is_char_boundary(end) {
        end -= 1;
    }
    redacted[..end].to_string()
}

enum LineVerdict {
    Keep { day: String, text: String },
    Invalid,
    Oversize,
    Future,
    Old,
}

impl ClientLogStore {
    /// `root` is the logs directory; client data lives in `<root>/clients`.
    pub fn open(root: &Path) -> std::io::Result<Self> {
        Self::open_with(root, Limits::default())
    }

    pub fn open_with(root: &Path, limits: Limits) -> std::io::Result<Self> {
        let clients = root.join("clients");
        fs::create_dir_all(&clients)?;
        Ok(Self {
            clients,
            limits,
            state: Mutex::new(State {
                daily: HashMap::new(),
            }),
        })
    }

    pub fn device_dir(&self, device: &AuthenticatedDevice) -> PathBuf {
        self.clients.join(device.id())
    }

    /// Highest acked cursor for the device (0 when unknown).
    pub fn last_cursor(&self, device: &AuthenticatedDevice) -> u64 {
        let _guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        self.read_cursor(device.id())
    }

    fn read_cursor(&self, id: &str) -> u64 {
        let path = self.clients.join(id).join("cursor.json");
        fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|value| value.get("cursor").and_then(Value::as_u64))
            .unwrap_or(0)
    }

    /// Record who the device is. Writes `device.json` with name, platform, app version and last seen.
    pub fn device_meta(
        &self,
        device: &AuthenticatedDevice,
        meta: &DeviceMeta,
    ) -> Result<(), IngestError> {
        self.device_meta_at(device, meta, SystemTime::now())
    }

    pub fn device_meta_at(
        &self,
        device: &AuthenticatedDevice,
        meta: &DeviceMeta,
        now: SystemTime,
    ) -> Result<(), IngestError> {
        let _guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let dir = self.clients.join(device.id());
        fs::create_dir_all(&dir)?;
        let doc = json!({
            "device": device.id(),
            "name": clamp_meta(&meta.name),
            "platform": clamp_meta(&meta.platform),
            "app_version": clamp_meta(&meta.app_version),
            "last_seen": logging::timestamp(now).0,
        });
        write_atomic(&dir.join("device.json"), doc.to_string().as_bytes())?;
        Ok(())
    }

    fn touch_last_seen(&self, dir: &Path, now: SystemTime) {
        let path = dir.join("device.json");
        let Ok(text) = fs::read_to_string(&path) else {
            return;
        };
        if let Ok(mut doc) = serde_json::from_str::<Value>(&text) {
            if let Some(map) = doc.as_object_mut() {
                map.insert("last_seen".into(), json!(logging::timestamp(now).0));
                let _ = write_atomic(&path, doc.to_string().as_bytes());
            }
        }
    }

    pub fn ingest(&self, device: &AuthenticatedDevice, batch: Batch) -> Result<Ack, IngestError> {
        self.ingest_at(device, batch, SystemTime::now())
    }

    pub fn ingest_at(
        &self,
        device: &AuthenticatedDevice,
        batch: Batch,
        now: SystemTime,
    ) -> Result<Ack, IngestError> {
        // Defence in depth: the type already validated the id.
        let id = sanitize_device_id(device.id())?;
        let size: usize = batch.lines.iter().map(|l| l.len() + 1).sum();
        if size > MAX_BATCH_BYTES {
            return Err(IngestError::BatchTooLarge { bytes: size });
        }
        if batch.cursor_to < batch.cursor_from {
            return Err(IngestError::InvalidCursor);
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let stored = self.read_cursor(id);
        if batch.cursor_to <= stored {
            return Ok(Ack {
                cursor: stored,
                duplicate: true,
                ..Ack::default()
            });
        }
        // Partial overlap with what is stored: skip the lines the host already has, when the counts line up.
        let mut lines = batch.lines.as_slice();
        if batch.cursor_from < stored && (batch.cursor_to - batch.cursor_from) == lines.len() as u64
        {
            lines = &lines[(stored - batch.cursor_from) as usize..];
        }

        let now_ms = epoch_ms(now);
        let mut ack = Ack::default();
        let mut by_day: Vec<(String, String)> = Vec::new();
        let mut bytes = 0u64;
        for raw in lines {
            match self.check_line(id, raw, now_ms) {
                LineVerdict::Keep { day, text } => {
                    bytes += text.len() as u64 + 1;
                    match by_day.iter_mut().find(|(d, _)| *d == day) {
                        Some((_, buf)) => {
                            buf.push_str(&text);
                            buf.push('\n');
                        }
                        None => by_day.push((day, format!("{text}\n"))),
                    }
                    ack.written += 1;
                }
                LineVerdict::Invalid => ack.dropped_invalid += 1,
                LineVerdict::Oversize => ack.dropped_oversize += 1,
                LineVerdict::Future => ack.dropped_future += 1,
                LineVerdict::Old => ack.dropped_old += 1,
            }
        }

        let today = now_ms.div_euclid(86_400_000);
        let entry = state.daily.entry(id.to_string()).or_insert((today, 0));
        if entry.0 != today {
            *entry = (today, 0);
        }
        if entry.1 + bytes > self.limits.daily_bytes {
            return Err(IngestError::RateLimited);
        }

        let dir = self.clients.join(id);
        fs::create_dir_all(&dir)?;
        for (day, buf) in &by_day {
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join(format!("{day}.jsonl")))?;
            file.write_all(buf.as_bytes())?;
        }
        entry.1 += bytes;

        let cursor_doc = json!({ "cursor": batch.cursor_to });
        write_atomic(&dir.join("cursor.json"), cursor_doc.to_string().as_bytes())?;
        ack.cursor = batch.cursor_to;
        self.touch_last_seen(&dir, now);
        self.enforce_caps(id);
        Ok(ack)
    }

    fn check_line(&self, id: &str, raw: &str, now_ms: i64) -> LineVerdict {
        if raw.len() > MAX_LINE_BYTES {
            return LineVerdict::Oversize;
        }
        let Ok(Value::Object(mut map)) = serde_json::from_str::<Value>(raw.trim()) else {
            return LineVerdict::Invalid;
        };
        let Some(ts) = map.get("ts").and_then(Value::as_str).map(str::to_string) else {
            return LineVerdict::Invalid;
        };
        let Some(ts_ms) = parse_ts_strict(&ts) else {
            return LineVerdict::Invalid;
        };
        let level_ok = map
            .get("level")
            .and_then(Value::as_str)
            .is_some_and(|l| LEVELS.contains(&l));
        let src_ok = map
            .get("src")
            .and_then(Value::as_str)
            .is_some_and(|s| SOURCES.contains(&s));
        let msg_ok = map.get("msg").is_some_and(Value::is_string);
        let optional_ok = ["device", "pc", "session", "target"]
            .iter()
            .all(|k| map.get(*k).is_none_or(|v| v.is_null() || v.is_string()))
            && map
                .get("fields")
                .is_none_or(|v| v.is_null() || v.is_object());
        if !(level_ok && src_ok && msg_ok && optional_ok) {
            return LineVerdict::Invalid;
        }
        if ts_ms > now_ms + FUTURE_SKEW_MS {
            return LineVerdict::Future;
        }
        let today = now_ms.div_euclid(86_400_000);
        if ts_ms.div_euclid(86_400_000) < today - self.limits.retention_days {
            return LineVerdict::Old;
        }
        // The host knows who sent it: never take the device field from the line.
        map.insert("device".into(), json!(id));
        let text = redact::redact_line(&Value::Object(map).to_string());
        if text.len() > MAX_LINE_BYTES || text.contains('\n') {
            return LineVerdict::Oversize;
        }
        LineVerdict::Keep {
            day: ts[..10].to_string(),
            text,
        }
    }

    /// Evict oldest day files until this device is under its cap, then until the total is under the cap.
    fn enforce_caps(&self, id: &str) {
        let mut files = day_files(&self.clients.join(id));
        files.sort_by_key(|(day, _, _)| *day);
        let mut total: u64 = files.iter().map(|(_, _, s)| *s).sum();
        for (_, path, size) in &files {
            if total <= self.limits.per_device_bytes {
                break;
            }
            if fs::remove_file(path).is_ok() {
                total -= size;
            }
        }
        let mut all: Vec<(i64, PathBuf, u64)> = Vec::new();
        for (_, dir) in device_dirs(&self.clients) {
            all.extend(day_files(&dir));
        }
        let mut grand: u64 = all.iter().map(|(_, _, s)| *s).sum();
        all.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        for (_, path, size) in &all {
            if grand <= self.limits.total_bytes {
                break;
            }
            if fs::remove_file(path).is_ok() {
                grand -= size;
            }
        }
    }

    /// Delete day files older than the retention window (relative to `now`).
    pub fn sweep(&self, now: SystemTime) -> SweepReport {
        let _guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let today = epoch_ms(now).div_euclid(86_400_000);
        let mut report = SweepReport::default();
        for (_, dir) in device_dirs(&self.clients) {
            for (day, path, size) in day_files(&dir) {
                if day < today - self.limits.retention_days && fs::remove_file(&path).is_ok() {
                    report.removed_files += 1;
                    report.removed_bytes += size;
                }
            }
        }
        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    pub(crate) struct TempDir(pub PathBuf);
    impl TempDir {
        pub fn new(tag: &str) -> Self {
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let dir = std::env::temp_dir()
                .join(format!("bp-client-logs-{tag}-{}-{n}", std::process::id()));
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

    // 2026-11-02T13:45:12.345Z
    fn now() -> SystemTime {
        UNIX_EPOCH
            + Duration::from_millis(parse_ts_strict("2026-11-02T13:45:12.345Z").unwrap() as u64)
    }

    fn dev(id: &str) -> AuthenticatedDevice {
        AuthenticatedDevice::new(id).unwrap()
    }

    fn line(ts: &str, msg: &str) -> String {
        json!({"ts": ts, "level": "info", "src": "app-win", "device": "spoofed", "pc": null,
               "session": null, "target": "t", "msg": msg, "fields": {}})
        .to_string()
    }

    fn batch(lines: Vec<String>, from: u64, to: u64) -> Batch {
        Batch {
            lines,
            cursor_from: from,
            cursor_to: to,
        }
    }

    #[test]
    fn device_id_sanitizing_and_traversal() {
        for bad in [
            "",
            "..",
            ".",
            "../x",
            "a/b",
            "a\\b",
            "a b",
            "a.b",
            "a:b",
            "C:\\x",
            "é",
            "nul",
            "COM1",
            "Lpt9",
            &"a".repeat(65),
        ] {
            assert!(sanitize_device_id(bad).is_err(), "{bad:?}");
            assert!(AuthenticatedDevice::new(bad).is_err(), "{bad:?}");
        }
        assert!(sanitize_device_id(&"a".repeat(64)).is_ok());
        assert!(sanitize_device_id("Abc_12-x").is_ok());
        assert!(sanitize_device_id("console").is_ok());
    }

    #[test]
    fn ingest_requires_authenticated_device_type() {
        // Documented rule: ingest takes `&AuthenticatedDevice`, which is only built from a validated id.
        let tmp = TempDir::new("auth");
        let store = ClientLogStore::open(&tmp.0).unwrap();
        let d = dev("phone-1");
        let ack = store
            .ingest_at(
                &d,
                batch(vec![line("2026-11-02T13:00:00.000Z", "hi")], 0, 1),
                now(),
            )
            .unwrap();
        assert_eq!(ack.written, 1);
        assert!(tmp.0.join("clients/phone-1/2026-11-02.jsonl").is_file());
        assert!(!tmp.0.join("clients/spoofed").exists());
    }

    #[test]
    fn written_line_has_authenticated_device_and_day_file() {
        let tmp = TempDir::new("line");
        let store = ClientLogStore::open(&tmp.0).unwrap();
        let d = dev("phone-1");
        store
            .ingest_at(
                &d,
                batch(
                    vec![
                        line("2026-11-01T23:59:59.999Z", "a"),
                        line("2026-11-02T00:00:00.000Z", "b"),
                    ],
                    0,
                    2,
                ),
                now(),
            )
            .unwrap();
        let one = fs::read_to_string(tmp.0.join("clients/phone-1/2026-11-01.jsonl")).unwrap();
        let two = fs::read_to_string(tmp.0.join("clients/phone-1/2026-11-02.jsonl")).unwrap();
        assert!(one.contains("\"msg\":\"a\"") && two.contains("\"msg\":\"b\""));
        let v: Value = serde_json::from_str(one.trim()).unwrap();
        assert_eq!(v["device"], "phone-1");
        assert!(one.ends_with('\n'));
    }

    #[test]
    fn invalid_future_old_and_oversize_lines_are_dropped_and_counted() {
        let tmp = TempDir::new("drops");
        let store = ClientLogStore::open(&tmp.0).unwrap();
        let d = dev("p");
        let big = line("2026-11-02T13:00:00.000Z", &"x".repeat(9000));
        let lines = vec![
            line("2026-11-02T13:00:00.000Z", "ok"),
            "not json".to_string(),
            "[1,2]".to_string(),
            json!({"ts":"2026-11-02T13:00:00.000Z","level":"info","src":"host"}).to_string(),
            json!({"ts":"2026-11-02T13:00:00.000Z","level":"loud","src":"host","msg":"x"})
                .to_string(),
            json!({"ts":"2026-11-02T13:00:00.000Z","level":"info","src":"evil","msg":"x"})
                .to_string(),
            json!({"ts":"yesterday","level":"info","src":"host","msg":"x"}).to_string(),
            line("2026-11-02T13:00:00Z", "no millis"),
            line("2026-11-03T13:00:00.000Z", "future"),
            line("2026-10-01T13:00:00.000Z", "old"),
            big,
        ];
        let n = lines.len() as u64;
        let ack = store.ingest_at(&d, batch(lines, 0, n), now()).unwrap();
        assert_eq!(ack.written, 1);
        assert_eq!(ack.dropped_invalid, 7);
        assert_eq!(ack.dropped_future, 1);
        assert_eq!(ack.dropped_old, 1);
        assert_eq!(ack.dropped_oversize, 1);
        assert_eq!(ack.cursor, n);
    }

    #[test]
    fn oversize_batch_is_rejected() {
        let tmp = TempDir::new("batch");
        let store = ClientLogStore::open(&tmp.0).unwrap();
        let d = dev("p");
        let lines: Vec<String> = (0..9)
            .map(|i| {
                line(
                    "2026-11-02T13:00:00.000Z",
                    &format!("{i}{}", "y".repeat(7500)),
                )
            })
            .collect();
        let err = store.ingest_at(&d, batch(lines, 0, 9), now()).unwrap_err();
        assert!(matches!(err, IngestError::BatchTooLarge { .. }));
        assert_eq!(store.last_cursor(&d), 0);
        assert!(!tmp.0.join("clients/p/2026-11-02.jsonl").exists());
    }

    #[test]
    fn redacts_planted_secrets_before_writing() {
        let tmp = TempDir::new("redact");
        let store = ClientLogStore::open(&tmp.0).unwrap();
        let d = dev("p");
        let blob = "A".repeat(48);
        let evil = json!({
            "ts": "2026-11-02T13:00:00.000Z", "level": "info", "src": "browser", "target": "t",
            "msg": format!("pairing_code=GRAPE-4821 Authorization: Bearer abc.def.ghi blob {blob} url https://x/y?token=SECRETTOK&a=1"),
            "fields": {"token": "TOPSECRETVALUE", "nested": {"password": "hunter2hunter2"}, "note": "Bearer zzzzzzzz"}
        })
        .to_string();
        store.ingest_at(&d, batch(vec![evil], 0, 1), now()).unwrap();
        let text = fs::read_to_string(tmp.0.join("clients/p/2026-11-02.jsonl")).unwrap();
        for leak in [
            "GRAPE-4821",
            "abc.def.ghi",
            &blob,
            "SECRETTOK",
            "TOPSECRETVALUE",
            "hunter2hunter2",
            "zzzzzzzz",
        ] {
            assert!(!text.contains(leak), "leaked {leak}: {text}");
        }
        assert!(text.contains("[redacted"));
    }

    #[test]
    fn cursor_resume_and_duplicate_batch() {
        let tmp = TempDir::new("cursor");
        let d = dev("p");
        let b1 = batch(
            vec![
                line("2026-11-02T13:00:00.000Z", "1"),
                line("2026-11-02T13:00:01.000Z", "2"),
            ],
            0,
            2,
        );
        {
            let store = ClientLogStore::open(&tmp.0).unwrap();
            let ack = store.ingest_at(&d, b1.clone(), now()).unwrap();
            assert_eq!((ack.cursor, ack.duplicate), (2, false));
        }
        // New process / reconnect: cursor is persisted.
        let store = ClientLogStore::open(&tmp.0).unwrap();
        assert_eq!(store.last_cursor(&d), 2);
        let ack = store.ingest_at(&d, b1, now()).unwrap();
        assert!(ack.duplicate);
        assert_eq!((ack.cursor, ack.written), (2, 0));
        // Partial overlap: only the new line is written.
        let b2 = batch(
            vec![
                line("2026-11-02T13:00:01.000Z", "2"),
                line("2026-11-02T13:00:02.000Z", "3"),
            ],
            1,
            3,
        );
        let ack = store.ingest_at(&d, b2, now()).unwrap();
        assert_eq!((ack.cursor, ack.written), (3, 1));
        let text = fs::read_to_string(tmp.0.join("clients/p/2026-11-02.jsonl")).unwrap();
        assert_eq!(text.lines().count(), 3);
        // Cursor never moves backwards.
        let old = batch(vec![line("2026-11-02T13:00:03.000Z", "old")], 0, 1);
        assert!(store.ingest_at(&d, old, now()).unwrap().duplicate);
        assert_eq!(store.last_cursor(&d), 3);
        // Invalid cursor.
        assert_eq!(
            store.ingest_at(&d, batch(vec![], 5, 4), now()).unwrap_err(),
            IngestError::InvalidCursor
        );
    }

    #[test]
    fn per_device_cap_evicts_oldest_day_files_first() {
        let tmp = TempDir::new("devcap");
        let limits = Limits {
            per_device_bytes: 700,
            ..Limits::default()
        };
        let store = ClientLogStore::open_with(&tmp.0, limits).unwrap();
        let d = dev("p");
        let mut cursor = 0;
        for day in ["2026-10-31", "2026-11-01", "2026-11-02"] {
            let l = line(&format!("{day}T12:00:00.000Z"), &"word ".repeat(30));
            cursor += 1;
            store
                .ingest_at(&d, batch(vec![l.clone(), l], cursor - 1, cursor), now())
                .unwrap();
        }
        let dir = tmp.0.join("clients/p");
        assert!(!dir.join("2026-10-31.jsonl").exists(), "oldest evicted");
        assert!(dir.join("2026-11-02.jsonl").exists());
        let total: u64 = day_files(&dir).iter().map(|f| f.2).sum();
        assert!(total <= 700);
    }

    #[test]
    fn total_cap_evicts_oldest_across_devices() {
        let tmp = TempDir::new("totcap");
        let limits = Limits {
            total_bytes: 700,
            ..Limits::default()
        };
        let store = ClientLogStore::open_with(&tmp.0, limits).unwrap();
        let l = |day: &str| line(&format!("{day}T12:00:00.000Z"), &"word ".repeat(30));
        store
            .ingest_at(
                &dev("a"),
                batch(vec![l("2026-10-30"), l("2026-10-30")], 0, 1),
                now(),
            )
            .unwrap();
        store
            .ingest_at(
                &dev("b"),
                batch(vec![l("2026-11-02"), l("2026-11-02")], 0, 1),
                now(),
            )
            .unwrap();
        store
            .ingest_at(
                &dev("c"),
                batch(vec![l("2026-11-02"), l("2026-11-02")], 0, 1),
                now(),
            )
            .unwrap();
        assert!(!tmp.0.join("clients/a/2026-10-30.jsonl").exists());
        assert!(tmp.0.join("clients/c/2026-11-02.jsonl").exists());
    }

    #[test]
    fn daily_rate_cap_refuses_with_typed_error() {
        let tmp = TempDir::new("rate");
        let limits = Limits {
            daily_bytes: 1000,
            ..Limits::default()
        };
        let store = ClientLogStore::open_with(&tmp.0, limits).unwrap();
        let d = dev("p");
        let l = line("2026-11-02T12:00:00.000Z", &"word ".repeat(60));
        store
            .ingest_at(&d, batch(vec![l.clone(), l.clone()], 0, 2), now())
            .unwrap();
        let err = store
            .ingest_at(&d, batch(vec![l.clone(), l.clone()], 2, 4), now())
            .unwrap_err();
        assert_eq!(err, IngestError::RateLimited);
        assert_eq!(store.last_cursor(&d), 2, "refused batch is not acked");
        // Another device is unaffected; the next UTC day resets the budget.
        store
            .ingest_at(&dev("q"), batch(vec![l.clone()], 0, 1), now())
            .unwrap();
        let next_day = now() + Duration::from_secs(86_400);
        store.ingest_at(&d, batch(vec![l], 2, 3), next_day).unwrap();
    }

    #[test]
    fn retention_sweep_removes_old_day_files_only() {
        let tmp = TempDir::new("sweep");
        let store = ClientLogStore::open(&tmp.0).unwrap();
        let d = dev("p");
        let dir = tmp.0.join("clients/p");
        fs::create_dir_all(&dir).unwrap();
        for name in [
            "2026-10-01.jsonl",
            "2026-10-18.jsonl",
            "2026-10-19.jsonl",
            "2026-11-02.jsonl",
            "device.json",
            "notes.txt",
        ] {
            fs::write(dir.join(name), "x\n").unwrap();
        }
        let report = store.sweep(now());
        assert_eq!(report.removed_files, 2);
        assert!(!dir.join("2026-10-01.jsonl").exists());
        assert!(!dir.join("2026-10-18.jsonl").exists());
        assert!(
            dir.join("2026-10-19.jsonl").exists(),
            "exactly 14 days old is kept"
        );
        assert!(dir.join("device.json").exists());
        assert!(dir.join("notes.txt").exists());
        let _ = d;
    }

    #[test]
    fn device_json_content_and_last_seen_touch() {
        let tmp = TempDir::new("meta");
        let store = ClientLogStore::open(&tmp.0).unwrap();
        let d = dev("phone-1");
        store
            .device_meta_at(
                &d,
                &DeviceMeta {
                    name: "Pixel 8".into(),
                    platform: "android".into(),
                    app_version: "1.2.3+45".into(),
                },
                now(),
            )
            .unwrap();
        let read = || -> Value {
            serde_json::from_str(
                &fs::read_to_string(tmp.0.join("clients/phone-1/device.json")).unwrap(),
            )
            .unwrap()
        };
        let v = read();
        assert_eq!(v["name"], "Pixel 8");
        assert_eq!(v["platform"], "android");
        assert_eq!(v["app_version"], "1.2.3+45");
        assert_eq!(v["last_seen"], "2026-11-02T13:45:12.345Z");
        let later = now() + Duration::from_secs(60);
        store
            .ingest_at(
                &d,
                batch(vec![line("2026-11-02T13:00:00.000Z", "x")], 0, 1),
                later,
            )
            .unwrap();
        assert_eq!(read()["last_seen"], "2026-11-02T13:46:12.345Z");
        assert_eq!(read()["name"], "Pixel 8");
    }

    #[test]
    fn time_parsing() {
        assert_eq!(parse_ts_strict("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(parse_ts_strict("1970-01-01T00:00:01.500Z"), Some(1500));
        assert_eq!(parse_ts_strict("1970-01-01T00:00:01Z"), None);
        assert_eq!(parse_ts_strict("2026-02-30T00:00:00.000Z"), None);
        assert_eq!(parse_rfc3339_millis("1970-01-01T01:00:00+01:00"), Some(0));
        assert_eq!(parse_rfc3339_millis("1970-01-01T00:00:00.5Z"), Some(500));
        assert_eq!(format_millis(1500), "1970-01-01T00:00:01.500Z");
        assert!(parse_day("2024-02-29").is_some());
        assert_eq!(parse_day("2025-02-29"), None);
    }
}
