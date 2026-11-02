//! Receiving side of the client log and feedback upload (wire protocol v1, see
//! docs/architecture/client-log-upload.md and fixtures/client-log-upload-contract.json).
//!
//! [`LogUpload`] is owned by the device-key host protocol. Its owner calls [`LogUpload::start`] when a
//! viewer authenticates, feeds the ctl messages of the six upload kinds to [`LogUpload::on_message`] and
//! drains the answers with [`LogUpload::poll`], sealing each as a ctl message. Everything is low priority:
//!
//! - the caller thread only parses, counts bytes and assembles feedback chunks in memory;
//! - all file I/O (cursor reads, [`ClientLogStore::ingest`], [`FeedbackStore::save`], duplicate lookups)
//!   happens on one worker thread behind a bounded queue; when the queue is full the answer is
//!   `rate-limited` (logs) or `retry` (feedback), never a wait;
//! - results carry the connection generation, so an answer for a viewer that is gone is discarded.
//!
//! The device id used for storage comes only from [`LogUpload::start`], that is from the authenticated
//! session. Any device id inside a message or a line is ignored (the stores overwrite it).

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use base64::Engine as _;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::client_logs::{
    check_cursor, check_stream, AuthenticatedDevice, Batch, ClientLogStore, IngestError,
};
use crate::feedback_store::{FeedbackError, FeedbackStore, MAX_PNG_BYTES};

/// The capability string, advertised by both sides.
pub const CAP_CLIENT_LOGS: &str = "client-logs-v1";
pub const WIRE_VERSION: u64 = 1;
pub const MAX_BATCH_BYTES: usize = 16 * 1024;
pub const MAX_BATCH_LINES: usize = 100;
pub const CONNECTION_BUDGET_BYTES: u64 = 8 * 1024 * 1024;
pub const MIN_BATCH_INTERVAL_MS: u64 = 500;
pub const MAX_FEEDBACK_PNG_BYTES: usize = MAX_PNG_BYTES;
pub const MAX_FEEDBACK_CHUNK_BYTES: usize = 32 * 1024;
pub const FEEDBACK_CHUNK_TIMEOUT: Duration = Duration::from_secs(60);
const JOB_QUEUE_CAPACITY: usize = 32;
const MAX_DONE_PER_POLL: usize = 64;
const RATE_LIMITED_RETRY_MS: u64 = 2_000;
const STORAGE_RETRY_MS: u64 = 60_000;
const REMEMBERED_REPORTS: usize = 64;
/// Longest accepted `data` string: base64 of a full chunk plus slack for padding.
const MAX_CHUNK_B64_CHARS: usize = MAX_FEEDBACK_CHUNK_BYTES.div_ceil(3) * 4 + 4;

/// The ctl kinds this module owns, in either direction.
pub fn is_upload_kind(kind: &str) -> bool {
    matches!(
        kind,
        "logs-state"
            | "logs-batch"
            | "logs-ack"
            | "feedback-begin"
            | "feedback-chunk"
            | "feedback-ack"
    )
}

// ---------------------------------------------------------------- wire messages

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogsStatus {
    Ok,
    RateLimited,
    Budget,
    Rejected,
}

impl LogsStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            LogsStatus::Ok => "ok",
            LogsStatus::RateLimited => "rate-limited",
            LogsStatus::Budget => "budget",
            LogsStatus::Rejected => "rejected",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedbackStatus {
    Ok,
    Rejected,
    Retry,
}

impl FeedbackStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            FeedbackStatus::Ok => "ok",
            FeedbackStatus::Rejected => "rejected",
            FeedbackStatus::Retry => "retry",
        }
    }
}

/// The limits the host announces in `logs-state`.
fn limits_json(limits: &UploadLimits) -> Value {
    json!({
        "maxBatchBytes": MAX_BATCH_BYTES,
        "maxBatchLines": MAX_BATCH_LINES,
        "connectionBudgetBytes": limits.connection_budget_bytes,
        "minBatchIntervalMs": MIN_BATCH_INTERVAL_MS,
    })
}

/// `logs-state`: the last accepted cursor of each stream the device has uploaded.
pub fn logs_state_message(cursors: &BTreeMap<String, String>, limits: &UploadLimits) -> Value {
    json!({"type": "logs-state", "v": WIRE_VERSION, "cursors": cursors, "limits": limits_json(limits)})
}

/// `logs-ack`. `reason` is set only for `rejected`.
pub fn logs_ack_message(
    stream: &str,
    id: u64,
    status: LogsStatus,
    cursor: &str,
    dropped: usize,
    retry_after_ms: u64,
    reason: Option<&str>,
) -> Value {
    let mut msg = json!({
        "type": "logs-ack", "v": WIRE_VERSION, "stream": stream, "id": id,
        "status": status.as_str(), "cursor": cursor, "dropped": dropped,
        "retryAfterMs": retry_after_ms,
    });
    if let (Some(reason), Some(map)) = (reason, msg.as_object_mut()) {
        map.insert("reason".into(), json!(reason));
    }
    msg
}

/// `feedback-ack` (`reason` is always present, empty when there is nothing to say).
pub fn feedback_ack_message(id: &str, status: FeedbackStatus, reason: &str) -> Value {
    json!({"type": "feedback-ack", "v": WIRE_VERSION, "id": id, "status": status.as_str(), "reason": reason})
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct LogsBatchMsg {
    pub stream: String,
    pub id: u64,
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub live: bool,
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackBeginMsg {
    pub id: String,
    pub report: Value,
    pub png_bytes: u64,
    pub chunks: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct FeedbackChunkMsg {
    pub id: String,
    pub i: u64,
    pub data: String,
}

// ---------------------------------------------------------------- configuration

#[derive(Clone, Copy)]
pub struct UploadLimits {
    /// Line bytes accepted per connection before the host answers `budget`.
    pub connection_budget_bytes: u64,
    /// Longest silence between the chunks of a feedback report.
    pub chunk_timeout: Duration,
    /// Jobs waiting for the worker.
    pub queue_capacity: usize,
    /// The clock the stores use for rate caps, retention and file names (tests fix it).
    pub clock: fn() -> SystemTime,
}

impl Default for UploadLimits {
    fn default() -> Self {
        Self {
            connection_budget_bytes: CONNECTION_BUDGET_BYTES,
            chunk_timeout: FEEDBACK_CHUNK_TIMEOUT,
            queue_capacity: JOB_QUEUE_CAPACITY,
            clock: SystemTime::now,
        }
    }
}

// ---------------------------------------------------------------- worker

enum Job {
    State {
        gen: u64,
        device: AuthenticatedDevice,
    },
    Batch {
        gen: u64,
        device: AuthenticatedDevice,
        id: u64,
        batch: Batch,
    },
    Feedback {
        gen: u64,
        device: AuthenticatedDevice,
        id: String,
        report: Value,
        png: Option<Vec<u8>>,
    },
}

struct Done {
    gen: u64,
    msg: Value,
    /// Cursors the host now holds, to answer budget and rate-limit acks without a disk read.
    cursors: Vec<(String, String)>,
    /// A report id that is on disk now.
    stored_report: Option<String>,
}

struct Worker {
    logs: ClientLogStore,
    feedback: FeedbackStore,
    limits: UploadLimits,
    stored: HashSet<(String, String)>,
    pause: Arc<Mutex<()>>,
}

impl Worker {
    fn run(mut self, jobs: Receiver<Job>, done: mpsc::Sender<Done>) {
        let pause = Arc::clone(&self.pause);
        for job in jobs {
            let _turn = pause.lock().unwrap_or_else(|e| e.into_inner());
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.handle(job)));
            match result {
                Ok(item) => {
                    if done.send(item).is_err() {
                        return;
                    }
                }
                Err(_) => tracing::warn!("client log upload worker job panicked"),
            }
        }
    }

    fn handle(&mut self, job: Job) -> Done {
        match job {
            Job::State { gen, device } => {
                let cursors = self.logs.cursors(&device);
                Done {
                    gen,
                    msg: logs_state_message(&cursors, &self.limits),
                    cursors: cursors.into_iter().collect(),
                    stored_report: None,
                }
            }
            Job::Batch {
                gen,
                device,
                id,
                batch,
            } => {
                let stream = batch.stream.clone();
                let now = (self.limits.clock)();
                let (msg, cursors) = match self.logs.ingest_at(&device, batch, now) {
                    Ok(ack) => (
                        logs_ack_message(
                            &stream,
                            id,
                            LogsStatus::Ok,
                            &ack.cursor,
                            ack.dropped_total(),
                            0,
                            None,
                        ),
                        vec![(stream, ack.cursor)],
                    ),
                    Err(error) => {
                        let stored = self.logs.last_cursor(&device, &stream).unwrap_or_default();
                        let (status, retry, reason) = match &error {
                            IngestError::RateLimited => {
                                (LogsStatus::RateLimited, STORAGE_RETRY_MS, None)
                            }
                            IngestError::Io(detail) => {
                                tracing::warn!(error = %detail, "client log upload could not be stored");
                                (LogsStatus::RateLimited, STORAGE_RETRY_MS, None)
                            }
                            IngestError::BatchTooLarge { .. } => {
                                (LogsStatus::Rejected, 0, Some("batch too large"))
                            }
                            IngestError::InvalidCursor => {
                                (LogsStatus::Rejected, 0, Some("invalid cursor"))
                            }
                            IngestError::InvalidStream => {
                                (LogsStatus::Rejected, 0, Some("unknown stream"))
                            }
                            IngestError::InvalidDeviceId => {
                                (LogsStatus::Rejected, 0, Some("invalid device"))
                            }
                        };
                        let cursors = if stored.is_empty() {
                            Vec::new()
                        } else {
                            vec![(stream.clone(), stored.clone())]
                        };
                        (
                            logs_ack_message(&stream, id, status, &stored, 0, retry, reason),
                            cursors,
                        )
                    }
                };
                Done {
                    gen,
                    msg,
                    cursors,
                    stored_report: None,
                }
            }
            Job::Feedback {
                gen,
                device,
                id,
                report,
                png,
            } => {
                let key = (device.id().to_string(), id.clone());
                let mut stored_report = None;
                let msg = if self.stored.contains(&key)
                    || self.feedback.contains_report(&device, &id)
                {
                    // A retry of a report that is already on disk: acknowledge, do not store again.
                    stored_report = Some(id.clone());
                    feedback_ack_message(&id, FeedbackStatus::Ok, "")
                } else {
                    let at = (self.limits.clock)();
                    match self.feedback.save_at(&device, report, png, at) {
                        Ok(_) => {
                            if self.stored.len() > 1024 {
                                self.stored.clear();
                            }
                            self.stored.insert(key);
                            stored_report = Some(id.clone());
                            feedback_ack_message(&id, FeedbackStatus::Ok, "")
                        }
                        Err(FeedbackError::Io(detail)) => {
                            tracing::warn!(error = %detail, "feedback report could not be stored");
                            feedback_ack_message(
                                &id,
                                FeedbackStatus::Retry,
                                "the PC could not store the report",
                            )
                        }
                        Err(error) => {
                            feedback_ack_message(&id, FeedbackStatus::Rejected, &error.to_string())
                        }
                    }
                };
                Done {
                    gen,
                    msg,
                    cursors: Vec::new(),
                    stored_report,
                }
            }
        }
    }
}

// ---------------------------------------------------------------- per-connection state

struct Assembly {
    id: String,
    report: Value,
    png_bytes: usize,
    chunks: usize,
    next: usize,
    buf: Vec<u8>,
    last: Instant,
}

struct Conn {
    device: AuthenticatedDevice,
    gen: u64,
    enabled: bool,
    state_pending: bool,
    budget_used: u64,
    cursors: BTreeMap<String, String>,
    feedback: Option<Assembly>,
}

/// What [`LogUpload::on_message`] could not use: a static reason for the caller's drop counter.
pub type Ignored = &'static str;

pub struct LogUpload {
    jobs: Option<SyncSender<Job>>,
    done: Receiver<Done>,
    worker: Option<JoinHandle<()>>,
    limits: UploadLimits,
    generation: u64,
    conn: Option<Conn>,
    reports: VecDeque<String>,
    ignored: u64,
    #[cfg_attr(not(test), allow(dead_code))]
    pause: Arc<Mutex<()>>,
}

impl LogUpload {
    /// `root` is the logs directory (`clients/` and `feedback/` live below it).
    pub fn open(root: &Path) -> std::io::Result<Self> {
        Self::open_with(root, UploadLimits::default())
    }

    pub fn open_with(root: &Path, limits: UploadLimits) -> std::io::Result<Self> {
        let logs = ClientLogStore::open(root)?;
        let feedback = FeedbackStore::open(root)?;
        Ok(Self::with_stores(logs, feedback, limits))
    }

    pub fn with_stores(
        logs: ClientLogStore,
        feedback: FeedbackStore,
        limits: UploadLimits,
    ) -> Self {
        let (jobs, job_rx) = mpsc::sync_channel(limits.queue_capacity.max(1));
        let (done_tx, done) = mpsc::channel();
        let pause = Arc::new(Mutex::new(()));
        let worker = Worker {
            logs,
            feedback,
            limits,
            stored: HashSet::new(),
            pause: Arc::clone(&pause),
        };
        let handle = std::thread::Builder::new()
            .name("client-log-upload".into())
            .spawn(move || worker.run(job_rx, done_tx))
            .ok();
        Self {
            jobs: handle.is_some().then_some(jobs),
            done,
            worker: handle,
            limits,
            generation: 0,
            conn: None,
            reports: VecDeque::new(),
            ignored: 0,
            pause,
        }
    }

    /// Messages ignored so far (unknown version, capability off, not authenticated, malformed).
    pub fn ignored(&self) -> u64 {
        self.ignored
    }

    /// Whether the current connection has the capability on.
    pub fn enabled(&self) -> bool {
        self.conn.as_ref().is_some_and(|conn| conn.enabled)
    }

    /// A viewer authenticated as `device_id` (taken from the session, never from a message).
    /// `offered` says its auth-hello already carried `client-logs-v1`. Replaces any earlier connection.
    pub fn start(&mut self, device_id: &str, offered: bool) {
        self.generation += 1;
        self.conn = match AuthenticatedDevice::new(device_id) {
            Ok(device) => Some(Conn {
                device,
                gen: self.generation,
                enabled: false,
                state_pending: false,
                budget_used: 0,
                cursors: BTreeMap::new(),
                feedback: None,
            }),
            Err(_) => {
                tracing::warn!("client log upload off: the authenticated device id is unusable");
                None
            }
        };
        if offered {
            self.offer();
        }
    }

    /// The viewer advertised `client-logs-v1` (auth-hello or its ctl `caps`). Idempotent. The
    /// first time, `logs-state` is queued.
    pub fn offer(&mut self) {
        let Some(conn) = self.conn.as_mut() else {
            return;
        };
        if conn.enabled {
            return;
        }
        conn.enabled = true;
        conn.state_pending = true;
        self.send_state();
    }

    /// The viewer is gone: drop the connection state and discard answers still in flight.
    pub fn stop(&mut self) {
        self.generation += 1;
        self.conn = None;
    }

    fn send_state(&mut self) {
        let (Some(conn), Some(jobs)) = (self.conn.as_mut(), self.jobs.as_ref()) else {
            return;
        };
        if !conn.state_pending {
            return;
        }
        let job = Job::State {
            gen: conn.gen,
            device: conn.device.clone(),
        };
        if jobs.try_send(job).is_ok() {
            conn.state_pending = false;
        }
    }

    fn cached_cursor(conn: &Conn, stream: &str) -> String {
        conn.cursors.get(stream).cloned().unwrap_or_default()
    }

    /// Handles one decrypted ctl message of an upload kind. Returns the answers to send now; slower
    /// answers arrive through [`LogUpload::poll`]. `Err` is a static reason for the drop counter.
    pub fn on_message(&mut self, kind: &str, text: &str) -> Result<Vec<Value>, Ignored> {
        self.on_message_at(kind, text, Instant::now())
    }

    pub fn on_message_at(
        &mut self,
        kind: &str,
        text: &str,
        now: Instant,
    ) -> Result<Vec<Value>, Ignored> {
        let result = self.handle_message(kind, text, now);
        if result.is_err() {
            self.ignored = self.ignored.saturating_add(1);
        }
        result
    }

    fn handle_message(
        &mut self,
        kind: &str,
        text: &str,
        now: Instant,
    ) -> Result<Vec<Value>, Ignored> {
        let Some(conn) = self.conn.as_ref() else {
            return Err("client log message before authentication");
        };
        if !conn.enabled {
            return Err("client log message without client-logs-v1");
        }
        let value: Value =
            serde_json::from_str(text).map_err(|_| "client log message is not JSON")?;
        if value.get("v").and_then(Value::as_u64) != Some(WIRE_VERSION) {
            return Err("client log message with an unknown version");
        }
        match kind {
            "logs-batch" => self.logs_batch(value),
            "feedback-begin" => self.feedback_begin(value, now),
            "feedback-chunk" => self.feedback_chunk(value, now),
            _ => Err("client sent a host-to-client log message"),
        }
    }

    fn logs_batch(&mut self, value: Value) -> Result<Vec<Value>, Ignored> {
        let batch: LogsBatchMsg = match serde_json::from_value(value.clone()) {
            Ok(batch) => batch,
            Err(_) => {
                // Answer when the batch is identifiable, so the client does not wait for an ack.
                let (Some(stream), Some(id)) = (
                    value.get("stream").and_then(Value::as_str),
                    value.get("id").and_then(Value::as_u64),
                ) else {
                    return Err("malformed client log batch");
                };
                let conn = self
                    .conn
                    .as_ref()
                    .ok_or("client log message before authentication")?;
                let cursor = Self::cached_cursor(conn, stream);
                return Ok(vec![logs_ack_message(
                    &truncate(stream, 32),
                    id,
                    LogsStatus::Rejected,
                    &cursor,
                    0,
                    0,
                    Some("malformed batch"),
                )]);
            }
        };
        let Some(conn) = self.conn.as_mut() else {
            return Err("client log message before authentication");
        };
        let cursor = Self::cached_cursor(conn, &batch.stream);
        let bytes: u64 = batch.lines.iter().map(|l| l.len() as u64 + 1).sum();
        let reply = |status, retry, reason: Option<&str>, stream: &str| {
            logs_ack_message(stream, batch.id, status, &cursor, 0, retry, reason)
        };
        // The per-connection byte budget comes first: past it nothing else matters.
        if conn.budget_used.saturating_add(bytes) > self.limits.connection_budget_bytes {
            return Ok(vec![reply(
                LogsStatus::Budget,
                0,
                None,
                &truncate(&batch.stream, 32),
            )]);
        }
        if check_stream(&batch.stream).is_err() {
            return Ok(vec![reply(
                LogsStatus::Rejected,
                0,
                Some("unknown stream"),
                &truncate(&batch.stream, 32),
            )]);
        }
        if check_cursor(&batch.from).is_err()
            || check_cursor(&batch.to).is_err()
            || batch.to < batch.from
        {
            return Ok(vec![reply(
                LogsStatus::Rejected,
                0,
                Some("invalid cursor"),
                &batch.stream,
            )]);
        }
        if batch.lines.len() > MAX_BATCH_LINES || bytes > MAX_BATCH_BYTES as u64 {
            return Ok(vec![reply(
                LogsStatus::Rejected,
                0,
                Some("batch too large"),
                &batch.stream,
            )]);
        }
        let Some(jobs) = self.jobs.as_ref() else {
            return Ok(vec![reply(
                LogsStatus::RateLimited,
                STORAGE_RETRY_MS,
                None,
                &batch.stream,
            )]);
        };
        let job = Job::Batch {
            gen: conn.gen,
            device: conn.device.clone(),
            id: batch.id,
            batch: Batch {
                stream: batch.stream.clone(),
                lines: batch.lines,
                cursor_from: batch.from,
                cursor_to: batch.to,
            },
        };
        match jobs.try_send(job) {
            Ok(()) => {
                conn.budget_used += bytes;
                Ok(Vec::new())
            }
            Err(TrySendError::Full(_)) => Ok(vec![reply(
                LogsStatus::RateLimited,
                RATE_LIMITED_RETRY_MS,
                None,
                &batch.stream,
            )]),
            Err(TrySendError::Disconnected(_)) => Ok(vec![reply(
                LogsStatus::RateLimited,
                STORAGE_RETRY_MS,
                None,
                &batch.stream,
            )]),
        }
    }

    fn remember_report(&mut self, id: &str) {
        if self.reports.iter().any(|known| known == id) {
            return;
        }
        if self.reports.len() >= REMEMBERED_REPORTS {
            self.reports.pop_front();
        }
        self.reports.push_back(id.to_string());
    }

    fn feedback_begin(&mut self, value: Value, now: Instant) -> Result<Vec<Value>, Ignored> {
        let begin: FeedbackBeginMsg = match serde_json::from_value(value.clone()) {
            Ok(begin) => begin,
            Err(_) => {
                let id = value
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| valid_report_id(id));
                return match id {
                    Some(id) => Ok(vec![feedback_ack_message(
                        id,
                        FeedbackStatus::Rejected,
                        "malformed report message",
                    )]),
                    None => Err("malformed feedback message"),
                };
            }
        };
        if !valid_report_id(&begin.id) {
            return Err("feedback message with an unusable report id");
        }
        let id = begin.id.clone();
        let rejected = |why: &str| {
            Ok(vec![feedback_ack_message(
                &id,
                FeedbackStatus::Rejected,
                why,
            )])
        };
        if begin.report.get("id").and_then(Value::as_str) != Some(id.as_str()) {
            return rejected("report id does not match");
        }
        if begin.png_bytes > MAX_FEEDBACK_PNG_BYTES as u64 {
            return rejected("screenshot too large");
        }
        let png_bytes = begin.png_bytes as usize;
        if begin.chunks != png_bytes.div_ceil(MAX_FEEDBACK_CHUNK_BYTES) as u64 {
            return rejected("chunk count does not match the screenshot size");
        }
        if self.reports.iter().any(|known| *known == id) {
            return Ok(vec![feedback_ack_message(&id, FeedbackStatus::Ok, "")]);
        }
        let timeout = self.limits.chunk_timeout;
        let Some(conn) = self.conn.as_mut() else {
            return Err("client log message before authentication");
        };
        if let Some(open) = &conn.feedback {
            if open.id != id && now.saturating_duration_since(open.last) < timeout {
                // One report at a time per device; the client retries after the first one finished.
                return Ok(vec![feedback_ack_message(
                    &id,
                    FeedbackStatus::Retry,
                    "another report is in progress",
                )]);
            }
            conn.feedback = None;
        }
        if png_bytes == 0 {
            return Ok(self.queue_feedback(id, begin.report, None));
        }
        conn.feedback = Some(Assembly {
            id,
            report: begin.report,
            png_bytes,
            chunks: begin.chunks as usize,
            next: 0,
            buf: Vec::with_capacity(png_bytes),
            last: now,
        });
        Ok(Vec::new())
    }

    fn feedback_chunk(&mut self, value: Value, now: Instant) -> Result<Vec<Value>, Ignored> {
        let chunk: FeedbackChunkMsg = match serde_json::from_value(value) {
            Ok(chunk) => chunk,
            Err(_) => return Err("malformed feedback chunk"),
        };
        if !valid_report_id(&chunk.id) {
            return Err("feedback message with an unusable report id");
        }
        let known = self.reports.iter().any(|known| *known == chunk.id);
        let Some(conn) = self.conn.as_mut() else {
            return Err("client log message before authentication");
        };
        let id = chunk.id.clone();
        let reply = |status, why: &str| Ok(vec![feedback_ack_message(&id, status, why)]);
        let Some(open) = conn.feedback.as_mut() else {
            // A chunk of a report that is already stored: the ack was lost, say ok again.
            return if known {
                reply(FeedbackStatus::Ok, "")
            } else {
                reply(FeedbackStatus::Rejected, "no report in progress")
            };
        };
        if open.id != chunk.id {
            return reply(FeedbackStatus::Rejected, "chunk belongs to another report");
        }
        let fail = |conn: &mut Conn, why: &str| {
            conn.feedback = None;
            Ok(vec![feedback_ack_message(
                &id,
                FeedbackStatus::Rejected,
                why,
            )])
        };
        if chunk.i != open.next as u64 {
            return fail(conn, "chunk out of order");
        }
        if chunk.data.len() > MAX_CHUNK_B64_CHARS {
            return fail(conn, "chunk too large");
        }
        let decoded = STANDARD
            .decode(chunk.data.as_bytes())
            .or_else(|_| STANDARD_NO_PAD.decode(chunk.data.as_bytes()));
        let Ok(bytes) = decoded else {
            return fail(conn, "chunk is not valid base64");
        };
        if bytes.len() > MAX_FEEDBACK_CHUNK_BYTES {
            return fail(conn, "chunk too large");
        }
        if open.buf.len() + bytes.len() > open.png_bytes {
            return fail(conn, "screenshot larger than announced");
        }
        open.buf.extend_from_slice(&bytes);
        open.next += 1;
        open.last = now;
        if open.next < open.chunks {
            return Ok(Vec::new());
        }
        if open.buf.len() != open.png_bytes {
            return fail(conn, "screenshot smaller than announced");
        }
        let Some(done) = conn.feedback.take() else {
            return Ok(Vec::new());
        };
        Ok(self.queue_feedback(done.id, done.report, Some(done.buf)))
    }

    /// Hands a complete report to the worker; the ack comes after the file is on disk.
    fn queue_feedback(&mut self, id: String, report: Value, png: Option<Vec<u8>>) -> Vec<Value> {
        let (Some(conn), Some(jobs)) = (self.conn.as_ref(), self.jobs.as_ref()) else {
            return vec![feedback_ack_message(
                &id,
                FeedbackStatus::Retry,
                "the PC is busy",
            )];
        };
        let job = Job::Feedback {
            gen: conn.gen,
            device: conn.device.clone(),
            id: id.clone(),
            report,
            png,
        };
        match jobs.try_send(job) {
            Ok(()) => Vec::new(),
            Err(_) => vec![feedback_ack_message(
                &id,
                FeedbackStatus::Retry,
                "the PC is busy",
            )],
        }
    }

    /// Answers that are ready (worker results, a feedback timeout, a deferred `logs-state`).
    pub fn poll(&mut self) -> Vec<Value> {
        self.poll_at(Instant::now())
    }

    pub fn poll_at(&mut self, now: Instant) -> Vec<Value> {
        self.send_state();
        let mut out = Vec::new();
        for item in self
            .done
            .try_iter()
            .take(MAX_DONE_PER_POLL)
            .collect::<Vec<_>>()
        {
            if item.gen != self.generation {
                continue;
            }
            let Some(conn) = self.conn.as_mut() else {
                continue;
            };
            for (stream, cursor) in item.cursors {
                conn.cursors.insert(stream, cursor);
            }
            if let Some(id) = item.stored_report {
                self.remember_report(&id);
            }
            out.push(item.msg);
        }
        let timeout = self.limits.chunk_timeout;
        if let Some(conn) = self.conn.as_mut() {
            if conn
                .feedback
                .as_ref()
                .is_some_and(|open| now.saturating_duration_since(open.last) >= timeout)
            {
                if let Some(open) = conn.feedback.take() {
                    out.push(feedback_ack_message(
                        &open.id,
                        FeedbackStatus::Retry,
                        "timed out waiting for the next chunk",
                    ));
                }
            }
        }
        out
    }

    /// Tests lock this to hold the worker between jobs.
    #[cfg(test)]
    pub(crate) fn pause_handle(&self) -> Arc<Mutex<()>> {
        Arc::clone(&self.pause)
    }
}

impl Drop for LogUpload {
    fn drop(&mut self) {
        // Closing the queue ends the worker after its current job.
        self.jobs = None;
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
    }
}

/// Report ids are UUIDs: 1-64 characters of `[A-Za-z0-9-]`.
fn valid_report_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn truncate(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

#[cfg(test)]
mod tests;
