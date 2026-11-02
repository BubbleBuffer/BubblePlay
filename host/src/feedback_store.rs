//! Host-side storage for feedback reports (spec 4): `<logs>/feedback/<UTC-ts>-<device-id>.json`
//! plus an optional `.png` with the same base name. No automatic deletion.
//!
//! The host validates the report shape, re-redacts every string and writes atomically (temp file, then a
//! no-overwrite hard link or rename), so a failed save leaves no partial file behind.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use serde_json::{json, Map, Value};

use crate::client_logs::AuthenticatedDevice;
use crate::logging::{self, redact};

pub const MAX_TEXT_BYTES: usize = 16 * 1024;
pub const MAX_LOG_ENTRIES: usize = 200;
pub const MAX_ERROR_ENTRIES: usize = 20;
pub const MAX_PNG_BYTES: usize = 5 * 1024 * 1024;
/// Upper bound for the serialized report (200 log lines of 8 KiB plus errors and text).
pub const MAX_REPORT_BYTES: usize = 2 * 1024 * 1024;
const KINDS: [&str; 4] = ["bug", "complaint", "idea", "other"];
const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedbackError {
    InvalidReport(&'static str),
    InvalidPng,
    PngTooLarge,
    Io(String),
}

impl std::fmt::Display for FeedbackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FeedbackError::InvalidReport(why) => write!(f, "invalid report: {why}"),
            FeedbackError::InvalidPng => write!(f, "screenshot is not a PNG"),
            FeedbackError::PngTooLarge => write!(f, "screenshot is too large"),
            FeedbackError::Io(msg) => write!(f, "io error: {msg}"),
        }
    }
}

impl std::error::Error for FeedbackError {}

impl From<std::io::Error> for FeedbackError {
    fn from(err: std::io::Error) -> Self {
        FeedbackError::Io(err.to_string())
    }
}

pub struct FeedbackStore {
    dir: PathBuf,
    lock: Mutex<()>,
}

/// `YYYYMMDDTHHMMSSmmmZ` for a time (colon-free, sortable).
pub fn file_timestamp(at: SystemTime) -> String {
    let (rfc, _) = logging::timestamp(at);
    // 2026-11-02T13:45:12.345Z -> 20261102T134512345Z
    let mut out = String::with_capacity(20);
    for ch in rfc.chars() {
        if ch.is_ascii_digit() || ch == 'T' || ch == 'Z' {
            out.push(ch);
        }
    }
    out
}

fn is_safe_app_text(text: &str) -> bool {
    text.len() <= 64
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'-' | b' '))
}

/// Validate and redact a report. Returns the cleaned object.
fn clean_report(report: Value) -> Result<Map<String, Value>, FeedbackError> {
    let Value::Object(mut map) = report else {
        return Err(FeedbackError::InvalidReport("report is not an object"));
    };
    if map.get("version").and_then(Value::as_u64) != Some(1) {
        return Err(FeedbackError::InvalidReport("version must be 1"));
    }
    match map.get("text") {
        Some(Value::String(text)) if text.len() <= MAX_TEXT_BYTES => {}
        Some(Value::String(_)) => return Err(FeedbackError::InvalidReport("text too long")),
        _ => return Err(FeedbackError::InvalidReport("text missing")),
    }
    let kind_ok = map
        .get("kind")
        .and_then(Value::as_str)
        .is_some_and(|k| KINDS.contains(&k));
    if !kind_ok {
        return Err(FeedbackError::InvalidReport(
            "kind must be bug, complaint, idea or other",
        ));
    }
    for (key, max, why) in [
        ("logs", MAX_LOG_ENTRIES, "too many log entries"),
        ("errors", MAX_ERROR_ENTRIES, "too many error entries"),
    ] {
        match map.get(key) {
            None | Some(Value::Null) => {}
            Some(Value::Array(items)) if items.len() <= max => {}
            Some(Value::Array(_)) => return Err(FeedbackError::InvalidReport(why)),
            Some(_) => {
                return Err(FeedbackError::InvalidReport(
                    "logs and errors must be arrays",
                ))
            }
        }
    }
    if map
        .iter()
        .map(|(k, v)| k.len() + v.to_string().len())
        .sum::<usize>()
        > MAX_REPORT_BYTES
    {
        return Err(FeedbackError::InvalidReport("report too large"));
    }

    // The screenshot travels as a separate PNG, never inline.
    map.remove("screenshot");
    // Build/commit strings are long hex-like ids that the blob rule would eat: keep only plain ones.
    if let Some(Value::Object(app)) = map.get_mut("app") {
        for value in app.values_mut() {
            if let Value::String(text) = value {
                if !is_safe_app_text(text) {
                    *value = Value::String(redact::REDACTED.to_string());
                }
            }
        }
    }
    let app = map.remove("app");
    let mut value = Value::Object(map);
    redact::redact_value(&mut value);
    let Value::Object(mut map) = value else {
        return Err(FeedbackError::InvalidReport("report is not an object"));
    };
    if let Some(app) = app {
        map.insert("app".into(), app);
    }
    Ok(map)
}

fn tmp_name(base: &str, ext: &str) -> String {
    format!(".{base}.{ext}.tmp")
}

fn write_tmp(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    let result = file.write_all(data).and_then(|()| file.sync_all());
    if result.is_err() {
        drop(file);
        let _ = fs::remove_file(path);
    }
    result
}

/// Publish `tmp` as `dest` without overwriting. `Ok(false)` means `dest` already exists.
fn publish(tmp: &Path, dest: &Path) -> std::io::Result<bool> {
    match fs::hard_link(tmp, dest) {
        Ok(()) => {
            let _ = fs::remove_file(tmp);
            Ok(true)
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(_) => {
            if dest.exists() {
                return Ok(false);
            }
            fs::rename(tmp, dest)?;
            Ok(true)
        }
    }
}

impl FeedbackStore {
    /// `root` is the logs directory; reports go to `<root>/feedback`.
    pub fn open(root: &Path) -> std::io::Result<Self> {
        let dir = root.join("feedback");
        fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            lock: Mutex::new(()),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn save(
        &self,
        device: &AuthenticatedDevice,
        report: Value,
        png: Option<Vec<u8>>,
    ) -> Result<PathBuf, FeedbackError> {
        self.save_at(device, report, png, SystemTime::now())
    }

    /// A report made on the PC itself (tray fallback), stored under the device id `this-pc`.
    pub fn save_local(
        &self,
        report: Value,
        png: Option<Vec<u8>>,
    ) -> Result<PathBuf, FeedbackError> {
        self.save(&AuthenticatedDevice::this_pc(), report, png)
    }

    pub fn save_at(
        &self,
        device: &AuthenticatedDevice,
        report: Value,
        png: Option<Vec<u8>>,
        at: SystemTime,
    ) -> Result<PathBuf, FeedbackError> {
        let mut map = clean_report(report)?;
        if let Some(png) = &png {
            if png.len() > MAX_PNG_BYTES {
                return Err(FeedbackError::PngTooLarge);
            }
            if png.len() < PNG_SIGNATURE.len() || png[..8] != PNG_SIGNATURE {
                return Err(FeedbackError::InvalidPng);
            }
        }
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        fs::create_dir_all(&self.dir)?;

        // Pick a free base name; on collision bump the millisecond so the name format stays the same.
        let mut when = at;
        for _ in 0..10_000 {
            let base = format!("{}-{}", file_timestamp(when), device.id());
            let json_path = self.dir.join(format!("{base}.json"));
            let png_path = self.dir.join(format!("{base}.png"));
            if json_path.exists() || png_path.exists() {
                when += Duration::from_millis(1);
                continue;
            }
            let tmp_png = self.dir.join(tmp_name(&base, "png"));
            let tmp_json = self.dir.join(tmp_name(&base, "json"));
            let cleanup = |paths: &[&Path]| {
                for path in paths {
                    let _ = fs::remove_file(path);
                }
            };

            map.insert("received".into(), json!(logging::timestamp(when).0));
            map.insert("received_from".into(), json!(device.id()));
            match &png {
                Some(_) => map.insert("screenshot".into(), json!(format!("{base}.png"))),
                None => map.remove("screenshot"),
            };
            let body = serde_json::to_vec_pretty(&Value::Object(map.clone()))
                .map_err(|e| FeedbackError::Io(e.to_string()))?;

            if let Some(png) = &png {
                if let Err(err) = write_tmp(&tmp_png, png) {
                    cleanup(&[&tmp_png]);
                    return Err(err.into());
                }
                match publish(&tmp_png, &png_path) {
                    Ok(true) => {}
                    Ok(false) => {
                        cleanup(&[&tmp_png]);
                        when += Duration::from_millis(1);
                        continue;
                    }
                    Err(err) => {
                        cleanup(&[&tmp_png]);
                        return Err(err.into());
                    }
                }
            }
            if let Err(err) = write_tmp(&tmp_json, &body) {
                cleanup(&[&tmp_json, &png_path]);
                return Err(err.into());
            }
            match publish(&tmp_json, &json_path) {
                Ok(true) => return Ok(json_path),
                Ok(false) => {
                    cleanup(&[&tmp_json]);
                    if png.is_some() {
                        cleanup(&[&png_path]);
                    }
                    when += Duration::from_millis(1);
                }
                Err(err) => {
                    cleanup(&[&tmp_json, &png_path]);
                    return Err(err.into());
                }
            }
        }
        Err(FeedbackError::Io("no free file name".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::UNIX_EPOCH;

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let dir = std::env::temp_dir().join(format!("bp-feedback-{}-{n}", std::process::id()));
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

    fn at() -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(1_793_626_712_345) // 2026-11-02T13:38:32.345Z
    }

    fn report() -> Value {
        json!({
            "version": 1, "id": "7f0e3c1a-0000-4000-8000-000000000000", "created": "2026-11-02T13:38:00.000Z",
            "text": "The stream froze\nsecond line", "kind": "bug",
            "app": {"version": "1.2.3", "build": "45", "commit": "0123456789abcdef0123456789abcdef01234567x"},
            "platform": "android", "device": {"id": "phone-1", "name": "Pixel", "model": "8"},
            "pc": "pc-9", "session": "s-1", "route": "session", "locale": "en-US",
            "logs": [{"ts": "2026-11-02T13:37:00.000Z", "level": "info", "msg": "hi"}],
            "errors": [], "screenshot": "AAAA"
        })
    }

    fn png() -> Vec<u8> {
        let mut v = PNG_SIGNATURE.to_vec();
        v.extend_from_slice(b"rest-of-image");
        v
    }

    fn files(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn round_trip_with_png() {
        let tmp = TempDir::new();
        let store = FeedbackStore::open(&tmp.0).unwrap();
        let d = AuthenticatedDevice::new("phone-1").unwrap();
        let path = store.save_at(&d, report(), Some(png()), at()).unwrap();
        assert_eq!(
            path.file_name().unwrap().to_str().unwrap(),
            "20261102T133832345Z-phone-1.json"
        );
        let png_path = path.with_extension("png");
        assert_eq!(fs::read(&png_path).unwrap(), png());
        let back: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back["text"], "The stream froze\nsecond line");
        assert_eq!(back["kind"], "bug");
        assert_eq!(back["received_from"], "phone-1");
        assert_eq!(back["received"], "2026-11-02T13:38:32.345Z");
        assert_eq!(back["screenshot"], "20261102T133832345Z-phone-1.png");
        assert_eq!(
            back["app"]["commit"],
            "0123456789abcdef0123456789abcdef01234567x"
        );
        assert_eq!(back["app"]["version"], "1.2.3");
        assert_eq!(back["pc"], "pc-9");
        assert_eq!(files(store.dir()).len(), 2, "no temp files left");
    }

    #[test]
    fn non_plain_app_text_is_redacted() {
        let tmp = TempDir::new();
        let store = FeedbackStore::open(&tmp.0).unwrap();
        let d = AuthenticatedDevice::new("phone-1").unwrap();
        let mut r = report();
        r["app"]["build"] = json!("45 token=abc/def");
        let path = store.save_at(&d, r, None, at()).unwrap();
        let back: Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(back["app"]["build"], "[redacted]");
    }

    #[test]
    fn without_png_drops_inline_screenshot_and_save_local_uses_this_pc() {
        let tmp = TempDir::new();
        let store = FeedbackStore::open(&tmp.0).unwrap();
        let path = store.save_local(report(), None).unwrap();
        assert!(path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .ends_with("-this-pc.json"));
        let back: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(back.get("screenshot").is_none());
        assert_eq!(files(store.dir()).len(), 1);
    }

    #[test]
    fn redacts_text_logs_and_errors() {
        let tmp = TempDir::new();
        let store = FeedbackStore::open(&tmp.0).unwrap();
        let d = AuthenticatedDevice::new("phone-1").unwrap();
        let blob = "B".repeat(50);
        let mut r = report();
        r["text"] = json!(format!("code=GRAPE-4821 Bearer tok.en.value {blob}"));
        r["logs"] = json!([
            {"msg": "url https://x/y?ticket=TICKETVALUE", "fields": {"password": "hunter2hunter2"}},
            format!("Authorization: Bearer LOGSECRET")
        ]);
        r["errors"] =
            json!([{"msg": "pairing=PAIRSECRET", "fields": {"stack": format!("at {blob}")}}]);
        let path = store.save_at(&d, r, None, at()).unwrap();
        let text = fs::read_to_string(path).unwrap();
        for leak in [
            "GRAPE-4821",
            "tok.en.value",
            &blob,
            "TICKETVALUE",
            "hunter2hunter2",
            "LOGSECRET",
            "PAIRSECRET",
        ] {
            assert!(!text.contains(leak), "leaked {leak}");
        }
    }

    #[test]
    fn validation_failures_leave_nothing_behind() {
        let tmp = TempDir::new();
        let store = FeedbackStore::open(&tmp.0).unwrap();
        let d = AuthenticatedDevice::new("phone-1").unwrap();
        let mut cases: Vec<Value> = Vec::new();
        cases.push(json!([1]));
        let mut r = report();
        r["version"] = json!(2);
        cases.push(r);
        let mut r = report();
        r["text"] = json!("x".repeat(MAX_TEXT_BYTES + 1));
        cases.push(r);
        let mut r = report();
        r.as_object_mut().unwrap().remove("text");
        cases.push(r);
        let mut r = report();
        r["kind"] = json!("praise");
        cases.push(r);
        let mut r = report();
        r["logs"] = json!(vec![json!({}); MAX_LOG_ENTRIES + 1]);
        cases.push(r);
        let mut r = report();
        r["errors"] = json!(vec![json!({}); MAX_ERROR_ENTRIES + 1]);
        cases.push(r);
        let mut r = report();
        r["logs"] = json!("nope");
        cases.push(r);
        for case in cases {
            assert!(matches!(
                store.save_at(&d, case, None, at()),
                Err(FeedbackError::InvalidReport(_))
            ));
        }
        // Boundary values pass.
        let mut r = report();
        r["text"] = json!("x".repeat(MAX_TEXT_BYTES));
        r["logs"] = json!(vec![json!({"msg": "a"}); MAX_LOG_ENTRIES]);
        r["errors"] = json!(vec![json!({"msg": "a"}); MAX_ERROR_ENTRIES]);
        store.save_at(&d, r, None, at()).unwrap();
        fs::remove_file(store.dir().join("20261102T133832345Z-phone-1.json")).unwrap();

        assert_eq!(
            store.save_at(&d, report(), Some(b"GIF89a....".to_vec()), at()),
            Err(FeedbackError::InvalidPng)
        );
        assert_eq!(
            store.save_at(&d, report(), Some(vec![]), at()),
            Err(FeedbackError::InvalidPng)
        );
        let mut big = PNG_SIGNATURE.to_vec();
        big.resize(MAX_PNG_BYTES + 1, 0);
        assert_eq!(
            store.save_at(&d, report(), Some(big), at()),
            Err(FeedbackError::PngTooLarge)
        );
        let mut exact = PNG_SIGNATURE.to_vec();
        exact.resize(MAX_PNG_BYTES, 0);
        store.save_at(&d, report(), Some(exact), at()).unwrap();
        for name in files(store.dir()) {
            fs::remove_file(store.dir().join(name)).unwrap();
        }
        assert!(files(store.dir()).is_empty());
    }

    #[test]
    fn failed_save_leaves_no_partial_files() {
        let tmp = TempDir::new();
        let store = FeedbackStore::open(&tmp.0).unwrap();
        let d = AuthenticatedDevice::new("phone-1").unwrap();
        // A directory squatting on the json temp name makes the json write fail after the png was published.
        let squat = store.dir().join(".20261102T133832345Z-phone-1.json.tmp");
        fs::create_dir(&squat).unwrap();
        let err = store.save_at(&d, report(), Some(png()), at()).unwrap_err();
        assert!(matches!(err, FeedbackError::Io(_)));
        assert_eq!(
            files(store.dir()),
            vec![squat.file_name().unwrap().to_string_lossy().into_owned()]
        );
        fs::remove_dir(&squat).unwrap();
        // Works again once the obstacle is gone, and leaves only the two final files.
        store.save_at(&d, report(), Some(png()), at()).unwrap();
        assert_eq!(files(store.dir()).len(), 2);
    }

    #[test]
    fn collisions_get_distinct_names() {
        let tmp = TempDir::new();
        let store = FeedbackStore::open(&tmp.0).unwrap();
        let d = AuthenticatedDevice::new("phone-1").unwrap();
        let a = store.save_at(&d, report(), Some(png()), at()).unwrap();
        let b = store.save_at(&d, report(), None, at()).unwrap();
        let c = store.save_at(&d, report(), Some(png()), at()).unwrap();
        assert_eq!(
            [a.clone(), b.clone(), c.clone()]
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            3
        );
        assert!(b
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("20261102T133832346Z"));
        assert!(c
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("20261102T133832347Z"));
        // First report was not overwritten.
        let first: Value = serde_json::from_str(&fs::read_to_string(&a).unwrap()).unwrap();
        assert_eq!(first["received"], "2026-11-02T13:38:32.345Z");
        assert!(a.with_extension("png").exists() && c.with_extension("png").exists());
        assert!(!b.with_extension("png").exists());
    }

    #[test]
    fn file_timestamp_is_colon_free() {
        let s = file_timestamp(at());
        assert_eq!(s, "20261102T133832345Z");
        assert!(!s.contains(':') && !s.contains('-') && !s.contains('.'));
    }
}
