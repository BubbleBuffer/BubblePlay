//! BubblePlay JSONL logging for the host-side processes (host, tray, admin agent, secure helper).
//!
//! One JSON object per line, day-rotated files `<src>-YYYY-MM-DD.jsonl` (plus `<src>-YYYY-MM-DD.N.jsonl`
//! when a day file reaches the size cap), time-based retention, a bounded drop-oldest queue drained
//! by one writer thread, redaction before writing (see [`redact`]) and a panic hook that writes an
//! error line (target `panic`, with a stack) and flushes.
//!
//! Entry points: [`init`] for a process, [`Logger`] for tests and embedding,
//! [`redact::redact_text`] / [`redact::redact_value`] / [`redact::redact_line`] for other modules
//! (client log ingestion, feedback storage), [`redact::register_secret`] for literal secrets.

pub mod redact;

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;

/// Maximum length of one written line (spec section 1).
pub const MAX_LINE_BYTES: usize = 8 * 1024;
/// Maximum length of `fields.stack`.
pub const MAX_STACK_BYTES: usize = 4 * 1024;
pub const TRUNCATED_MARK: &str = "...[truncated]";
/// Own logs are kept this many full days (files older than today minus this are deleted).
pub const DEFAULT_RETENTION_DAYS: i64 = 7;
/// At most this many files per source.
pub const DEFAULT_MAX_FILES: usize = 20;
/// A day file rolls over to `.N` at this size.
pub const DEFAULT_MAX_FILE_BYTES: u64 = 20 * 1024 * 1024;
pub const DEFAULT_QUEUE_CAPACITY: usize = 4096;
const DROP_REPORT_EVERY: Duration = Duration::from_secs(60);

/// Settings for one process's log.
#[derive(Debug, Clone)]
pub struct LogConfig {
    /// `host`, `tray`, `agent` or `helper`: the `src` field and the file name prefix.
    pub src: &'static str,
    pub dir: PathBuf,
    pub retention_days: i64,
    pub max_files: usize,
    pub max_file_bytes: u64,
    pub queue_capacity: usize,
    /// Also print human-readable lines to stderr (the host does: the tray reads them).
    pub stderr: bool,
}

impl LogConfig {
    pub fn new(src: &'static str, dir: PathBuf) -> Self {
        Self {
            src,
            dir,
            retention_days: DEFAULT_RETENTION_DAYS,
            max_files: DEFAULT_MAX_FILES,
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            queue_capacity: DEFAULT_QUEUE_CAPACITY,
            stderr: false,
        }
    }
}

/// `%LOCALAPPDATA%\BubblePlay\logs`, overridable with `BUBBLEPLAY_LOG_DIR`, or
/// `<BUBBLEPLAY_TRAY_DATA_DIR>\logs` for isolated test instances (same as the tray).
pub fn default_log_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("BUBBLEPLAY_LOG_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    if let Some(dir) = std::env::var_os("BUBBLEPLAY_TRAY_DATA_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir).join("logs");
    }
    match std::env::var_os("LOCALAPPDATA").filter(|d| !d.is_empty()) {
        Some(base) => PathBuf::from(base).join("BubblePlay").join("logs"),
        None => std::env::temp_dir().join("BubblePlay").join("logs"),
    }
}

// ---------------------------------------------------------------- time

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = i64::from(if m > 2 { m - 3 } else { m + 9 });
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// UTC RFC3339 with exactly three fractional digits and `Z`, plus days since the Unix epoch.
pub fn timestamp(at: SystemTime) -> (String, i64) {
    let since = at.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
    let secs = since.as_secs() as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let ts = format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60,
        since.subsec_millis()
    );
    (ts, days)
}

pub fn now_rfc3339() -> String {
    timestamp(SystemTime::now()).0
}

// ---------------------------------------------------------------- line rendering

fn level_str(level: &Level) -> &'static str {
    match *level {
        Level::TRACE => "trace",
        Level::DEBUG => "debug",
        Level::INFO => "info",
        Level::WARN => "warn",
        Level::ERROR => "error",
    }
}

const KEY_ORDER: [&str; 9] = [
    "ts", "level", "src", "device", "pc", "session", "target", "msg", "fields",
];

/// Serialize a line object with the spec's key order first; unknown keys follow.
pub fn render_line(value: &Value) -> String {
    let Value::Object(map) = value else {
        return value.to_string();
    };
    let mut out = String::with_capacity(256);
    out.push('{');
    let mut first = true;
    let mut put = |out: &mut String, key: &str, value: &Value| {
        if !first {
            out.push(',');
        }
        first = false;
        out.push_str(&Value::String(key.to_string()).to_string());
        out.push(':');
        out.push_str(&value.to_string());
    };
    for key in KEY_ORDER {
        if let Some(value) = map.get(key) {
            put(&mut out, key, value);
        }
    }
    for (key, value) in map {
        if !KEY_ORDER.contains(&key.as_str()) {
            put(&mut out, key, value);
        }
    }
    out.push('}');
    out
}

fn truncate_on_boundary(text: &mut String, max: usize) {
    if text.len() <= max {
        return;
    }
    let mut cut = max;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    text.truncate(cut);
}

/// Truncate `text` to `max` bytes in total including the truncation marker.
pub fn truncate_with_mark(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut out = text.to_string();
    truncate_on_boundary(&mut out, max.saturating_sub(TRUNCATED_MARK.len()));
    out.push_str(TRUNCATED_MARK);
    out
}

/// Render `value` as a line of at most `max` bytes: shorten `msg`, then drop `fields.stack`,
/// then drop `fields`.
pub fn fit_line(value: &mut Value, max: usize) -> String {
    let mut line = render_line(value);
    if line.len() <= max {
        return line;
    }
    let overflow = line.len() - max;
    if let Some(Value::String(msg)) = value.get_mut("msg") {
        let target = msg.len().saturating_sub(overflow);
        // Escaping can make the rendered form longer than the raw text; leave slack.
        let target = target.saturating_sub(TRUNCATED_MARK.len() + 8);
        let mut shortened = std::mem::take(msg);
        truncate_on_boundary(&mut shortened, target);
        shortened.push_str(TRUNCATED_MARK);
        *msg = shortened;
    }
    line = render_line(value);
    if line.len() <= max {
        return line;
    }
    if let Some(Value::Object(fields)) = value.get_mut("fields") {
        fields.remove("stack");
    }
    line = render_line(value);
    if line.len() <= max {
        return line;
    }
    if let Some(map) = value.as_object_mut() {
        map.insert("fields".to_string(), json!({"truncated": true}));
    }
    line = render_line(value);
    while line.len() > max {
        // Last resort: halve the message until the line fits.
        let Some(Value::String(msg)) = value.get_mut("msg") else {
            break;
        };
        if msg.is_empty() {
            break;
        }
        let half = msg.len() / 2;
        truncate_on_boundary(msg, half);
        line = render_line(value);
    }
    line
}

// ---------------------------------------------------------------- files and retention

fn day_file_name(prefix: &str, day: i64, index: u32) -> String {
    let (y, m, d) = civil_from_days(day);
    if index == 0 {
        format!("{prefix}-{y:04}-{m:02}-{d:02}.jsonl")
    } else {
        format!("{prefix}-{y:04}-{m:02}-{d:02}.{index}.jsonl")
    }
}

/// Parse `<prefix>-YYYY-MM-DD[.N].jsonl` into (days since epoch, index).
fn parse_day_file(prefix: &str, name: &str) -> Option<(i64, u32)> {
    let rest = name.strip_prefix(prefix)?.strip_prefix('-')?;
    let rest = rest.strip_suffix(".jsonl")?;
    let (date, index) = match rest.split_once('.') {
        Some((date, index)) => (date, index.parse::<u32>().ok()?),
        None => (rest, 0),
    };
    let mut parts = date.split('-');
    let y = parts.next()?;
    let m = parts.next()?;
    let d = parts.next()?;
    if parts.next().is_some() || y.len() != 4 || m.len() != 2 || d.len() != 2 {
        return None;
    }
    let (y, m, d) = (
        y.parse::<i64>().ok()?,
        m.parse::<u32>().ok()?,
        d.parse::<u32>().ok()?,
    );
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some((days_from_civil(y, m, d), index))
}

/// Delete day files older than `retention_days` before `today` (days since epoch), then the
/// oldest files beyond `max_files`. Returns how many files were deleted. Never fails; the file
/// named by `keep` is never deleted.
pub fn apply_retention(
    dir: &Path,
    prefix: &str,
    today: i64,
    retention_days: i64,
    max_files: usize,
    keep: Option<&Path>,
) -> usize {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let mut files: Vec<(i64, u32, PathBuf)> = entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let (day, index) = parse_day_file(prefix, &name)?;
            Some((day, index, entry.path()))
        })
        .collect();
    let mut deleted = 0;
    files.retain(|(day, _, path)| {
        if *day < today - retention_days
            && Some(path.as_path()) != keep
            && fs::remove_file(path).is_ok()
        {
            deleted += 1;
            return false;
        }
        true
    });
    files.sort();
    let excess = files.len().saturating_sub(max_files.max(1));
    for (_, _, path) in files.iter().take(excess) {
        if Some(path.as_path()) != keep && fs::remove_file(path).is_ok() {
            deleted += 1;
        }
    }
    deleted
}

struct Current {
    day: i64,
    path: PathBuf,
    file: File,
    size: u64,
}

struct Writer {
    dir: PathBuf,
    prefix: &'static str,
    retention_days: i64,
    max_files: usize,
    max_file_bytes: u64,
    current: Option<Current>,
}

impl Writer {
    fn open_day(&mut self, day: i64) -> bool {
        let _ = fs::create_dir_all(&self.dir);
        let mut index = 0u32;
        loop {
            let path = self.dir.join(day_file_name(self.prefix, day, index));
            let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            if size >= self.max_file_bytes && index < 10_000 {
                index += 1;
                continue;
            }
            return match OpenOptions::new().create(true).append(true).open(&path) {
                Ok(file) => {
                    self.current = Some(Current {
                        day,
                        path: path.clone(),
                        file,
                        size,
                    });
                    apply_retention(
                        &self.dir,
                        self.prefix,
                        day,
                        self.retention_days,
                        self.max_files,
                        Some(&path),
                    );
                    true
                }
                Err(_) => {
                    self.current = None;
                    false
                }
            };
        }
    }

    fn roll_for_size(&mut self, day: i64) {
        let Some(current) = self.current.as_ref() else {
            return;
        };
        let (cur_day, path) = (current.day, current.path.clone());
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let index = parse_day_file(self.prefix, &name)
            .map(|(_, i)| i)
            .unwrap_or(0);
        let next = self
            .dir
            .join(day_file_name(self.prefix, cur_day, index + 1));
        match OpenOptions::new().create(true).append(true).open(&next) {
            Ok(file) => {
                self.current = Some(Current {
                    day: cur_day,
                    path: next.clone(),
                    file,
                    size: 0,
                });
                apply_retention(
                    &self.dir,
                    self.prefix,
                    day.max(cur_day),
                    self.retention_days,
                    self.max_files,
                    Some(&next),
                );
            }
            Err(_) => self.current = None,
        }
    }

    fn write(&mut self, items: &[Item]) {
        for item in items {
            let needs_day = match &self.current {
                None => true,
                Some(current) => item.day > current.day,
            };
            if needs_day && !self.open_day(item.day) {
                continue;
            }
            let over = self.current.as_ref().is_some_and(|c| {
                c.size > 0 && c.size + item.line.len() as u64 + 1 > self.max_file_bytes
            });
            if over {
                self.roll_for_size(item.day);
            }
            let Some(current) = self.current.as_mut() else {
                continue;
            };
            let mut bytes = Vec::with_capacity(item.line.len() + 1);
            bytes.extend_from_slice(item.line.as_bytes());
            bytes.push(b'\n');
            match current.file.write_all(&bytes) {
                Ok(()) => current.size += bytes.len() as u64,
                Err(_) => self.current = None,
            }
        }
        if let Some(current) = self.current.as_mut() {
            let _ = current.file.flush();
        }
    }
}

// ---------------------------------------------------------------- logger

struct Item {
    day: i64,
    line: String,
}

struct Shared {
    src: &'static str,
    capacity: usize,
    device: Mutex<Option<String>>,
    /// The active session's id, used for lines that carry no `session` field of their own.
    session: Mutex<Option<String>>,
    queue: Mutex<VecDeque<Item>>,
    wake: Condvar,
    writer: Mutex<Writer>,
    dropped: AtomicU64,
    closing: AtomicBool,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// A running log: queue, writer thread and files. Cheap to clone.
#[derive(Clone)]
pub struct Logger {
    shared: Arc<Shared>,
    thread: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl Logger {
    pub fn new(config: LogConfig) -> Logger {
        let shared = Arc::new(Shared {
            src: config.src,
            capacity: config.queue_capacity.max(1),
            device: Mutex::new(None),
            session: Mutex::new(None),
            queue: Mutex::new(VecDeque::new()),
            wake: Condvar::new(),
            writer: Mutex::new(Writer {
                dir: config.dir,
                prefix: config.src,
                retention_days: config.retention_days,
                max_files: config.max_files,
                max_file_bytes: config.max_file_bytes.max(1024),
                current: None,
            }),
            dropped: AtomicU64::new(0),
            closing: AtomicBool::new(false),
        });
        let worker = shared.clone();
        let handle = std::thread::Builder::new()
            .name("bp-log-writer".into())
            .spawn(move || writer_loop(worker))
            .ok();
        Logger {
            shared,
            thread: Arc::new(Mutex::new(handle)),
        }
    }

    /// The tracing layer that writes events as JSONL lines.
    pub fn layer(&self) -> JsonlLayer {
        JsonlLayer {
            logger: self.clone(),
        }
    }

    /// Set this process's own device id (the `device` field of later lines).
    pub fn set_device(&self, device: Option<&str>) {
        *lock(&self.shared.device) = device.map(str::to_string);
    }

    /// Set the active session's id: later lines without their own `session` field carry it,
    /// from every thread. `None` ends it.
    pub fn set_session(&self, session: Option<&str>) {
        *lock(&self.shared.session) = session.map(str::to_string);
    }

    /// Write one line. Redacted, fitted to 8 KiB, queued; never panics and never blocks on I/O.
    pub fn log(&self, level: Level, target: &str, msg: &str, fields: Map<String, Value>) {
        self.log_at(SystemTime::now(), level, target, msg, fields);
    }

    pub fn log_at(
        &self,
        at: SystemTime,
        level: Level,
        target: &str,
        msg: &str,
        mut fields: Map<String, Value>,
    ) {
        let (ts, day) = timestamp(at);
        let pc = fields.remove("pc");
        let session = fields
            .remove("session")
            .or_else(|| lock(&self.shared.session).clone().map(Value::String));
        let device_field = fields.remove("device");
        let device = match device_field {
            Some(value) => value,
            None => lock(&self.shared.device)
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        };
        let mut value = json!({
            "ts": ts,
            "level": level_str(&level),
            "src": self.shared.src,
            "device": device,
            "pc": pc.unwrap_or(Value::Null),
            "session": session.unwrap_or(Value::Null),
            "target": target,
            "msg": msg,
        });
        if !fields.is_empty() {
            value["fields"] = Value::Object(fields);
        }
        let ts_for_failure = ts_of(&value);
        let line = catch_unwind(AssertUnwindSafe(|| {
            redact::redact_value(&mut value);
            fit_line(&mut value, MAX_LINE_BYTES)
        }))
        .unwrap_or_else(|_| redact::failed_line(&ts_for_failure));
        self.enqueue(Item { day, line });
    }

    fn enqueue(&self, item: Item) {
        {
            let mut queue = lock(&self.shared.queue);
            if queue.len() >= self.shared.capacity {
                queue.pop_front();
                self.shared.dropped.fetch_add(1, Ordering::Relaxed);
            }
            queue.push_back(item);
        }
        self.shared.wake.notify_one();
    }

    /// Write everything queued now, on the calling thread. Gives up after about two seconds.
    pub fn flush(&self) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match self.shared.writer.try_lock() {
                Ok(mut writer) => {
                    let batch: Vec<Item> = lock(&self.shared.queue).drain(..).collect();
                    writer.write(&batch);
                    return;
                }
                Err(std::sync::TryLockError::Poisoned(poisoned)) => {
                    let mut writer = poisoned.into_inner();
                    let batch: Vec<Item> = lock(&self.shared.queue).drain(..).collect();
                    writer.write(&batch);
                    return;
                }
                Err(std::sync::TryLockError::WouldBlock) => {
                    if Instant::now() >= deadline {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }

    /// Record a panic as an error line (target `panic`, `fields.stack`) and flush it.
    pub fn log_panic(&self, message: &str, location: Option<&str>, thread: &str, stack: &str) {
        let mut fields = Map::new();
        fields.insert("error".into(), Value::String(message.to_string()));
        fields.insert(
            "stack".into(),
            Value::String(truncate_with_mark(stack, MAX_STACK_BYTES)),
        );
        fields.insert("thread".into(), Value::String(thread.to_string()));
        if let Some(location) = location {
            fields.insert("location".into(), Value::String(location.to_string()));
        }
        let msg = format!("panic: {message}");
        self.log(Level::ERROR, "panic", &msg, fields);
        self.flush();
    }

    /// Stop the writer thread after writing what is queued.
    pub fn shutdown(&self) {
        self.shared.closing.store(true, Ordering::SeqCst);
        self.shared.wake.notify_all();
        let handle = lock(&self.thread).take();
        if let Some(handle) = handle {
            let _ = handle.join();
        }
        self.flush();
    }

    /// Number of lines dropped from a full queue and not yet reported (tests).
    pub fn pending_dropped(&self) -> u64 {
        self.shared.dropped.load(Ordering::Relaxed)
    }
}

fn ts_of(value: &Value) -> String {
    value
        .get("ts")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(now_rfc3339)
}

fn writer_loop(shared: Arc<Shared>) {
    let mut last_drop_report = Instant::now();
    loop {
        {
            let queue = lock(&shared.queue);
            if queue.is_empty() && !shared.closing.load(Ordering::SeqCst) {
                let _ = shared.wake.wait_timeout(queue, Duration::from_secs(1));
            }
        }
        // Take the writer lock before draining, so a `flush()` on another thread either runs
        // first and writes everything, or waits until this batch is on disk.
        {
            let mut writer = lock(&shared.writer);
            let batch: Vec<Item> = lock(&shared.queue).drain(..).collect();
            if !batch.is_empty() {
                writer.write(&batch);
            }
        }
        if last_drop_report.elapsed() >= DROP_REPORT_EVERY {
            last_drop_report = Instant::now();
            let dropped = shared.dropped.swap(0, Ordering::Relaxed);
            if dropped > 0 {
                let logger = Logger {
                    shared: shared.clone(),
                    thread: Arc::new(Mutex::new(None)),
                };
                let mut fields = Map::new();
                fields.insert("dropped".into(), json!(dropped));
                logger.log(
                    Level::WARN,
                    "logging",
                    "log lines dropped: queue was full",
                    fields,
                );
            }
        }
        if shared.closing.load(Ordering::SeqCst) && lock(&shared.queue).is_empty() {
            break;
        }
    }
}

// ---------------------------------------------------------------- tracing layer

#[derive(Default)]
struct EventVisitor {
    message: Option<String>,
    fields: Map<String, Value>,
}

impl EventVisitor {
    fn put(&mut self, field: &Field, value: Value) {
        self.fields.insert(field.name().to_string(), value);
    }
}

fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let _ = write!(text, ": {cause}");
        source = cause.source();
    }
    text
}

impl Visit for EventVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(value.to_string());
        } else {
            self.put(field, Value::String(value.to_string()));
        }
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.put(field, json!(value));
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.put(field, json!(value));
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.put(field, json!(value));
    }
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.put(field, json!(value));
    }
    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.put(field, Value::String(error_chain(value)));
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = Some(format!("{value:?}"));
        } else {
            self.put(field, Value::String(format!("{value:?}")));
        }
    }
}

/// Tracing layer produced by [`Logger::layer`].
pub struct JsonlLayer {
    logger: Logger,
}

impl<S: Subscriber> Layer<S> for JsonlLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = EventVisitor::default();
        event.record(&mut visitor);
        let metadata = event.metadata();
        self.logger.log(
            *metadata.level(),
            metadata.target(),
            &visitor.message.unwrap_or_default(),
            visitor.fields,
        );
    }
}

// ---------------------------------------------------------------- process setup

static GLOBAL: OnceLock<Logger> = OnceLock::new();

/// The process logger set by [`init`], if any.
pub fn global() -> Option<&'static Logger> {
    GLOBAL.get()
}

/// Set the process's own device id for later lines (no-op before [`init`]).
pub fn set_device(device: Option<&str>) {
    if let Some(logger) = global() {
        logger.set_device(device);
    }
}

/// Set the active session's id for later lines that carry no `session` field (no-op before
/// [`init`]). `None` ends the session.
pub fn set_session(session: Option<&str>) {
    if let Some(logger) = global() {
        logger.set_session(session);
    }
}

/// Flush the process logger (no-op before [`init`]).
pub fn flush() {
    if let Some(logger) = global() {
        logger.flush();
    }
}

/// Flushes and stops the writer on drop. Keep it alive for the life of the process.
pub struct LogGuard {
    logger: Logger,
}

impl LogGuard {
    pub fn logger(&self) -> &Logger {
        &self.logger
    }
}

impl Drop for LogGuard {
    fn drop(&mut self) {
        self.logger.shutdown();
    }
}

fn panic_message(info: &std::panic::PanicHookInfo<'_>) -> String {
    let payload = info.payload();
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_string()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "panic with a non-text payload".to_string()
    }
}

/// Chain a panic hook that writes an error line (target `panic`, stack) to `logger`, flushes,
/// then runs the previous hook (stderr output).
pub fn install_panic_hook(logger: &Logger) {
    let logger = logger.clone();
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = panic_message(info);
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()));
        let thread = std::thread::current();
        let stack = std::backtrace::Backtrace::force_capture().to_string();
        logger.log_panic(
            &message,
            location.as_deref(),
            thread.name().unwrap_or("unnamed"),
            &stack,
        );
        previous(info);
    }));
}

/// Start JSONL logging for this process: tracing subscriber (env filter, default `info`),
/// the JSONL file layer, optional stderr output and the panic hook. Returns `None` only when a
/// global subscriber already exists (the panic hook is still installed).
pub fn init(config: LogConfig) -> Option<LogGuard> {
    let stderr = config.stderr;
    let logger = Logger::new(config);
    let _ = GLOBAL.set(logger.clone());
    install_panic_hook(&logger);
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let stderr_layer = stderr.then(|| {
        tracing_subscriber::fmt::layer()
            .with_writer(|| std::io::LineWriter::new(std::io::stderr()))
            .with_ansi(false)
    });
    let result = tracing_subscriber::registry()
        .with(filter)
        .with(stderr_layer)
        .with(logger.layer())
        .try_init();
    match result {
        Ok(()) => Some(LogGuard { logger }),
        Err(_) => {
            logger.shutdown();
            None
        }
    }
}

#[cfg(test)]
mod tests;
