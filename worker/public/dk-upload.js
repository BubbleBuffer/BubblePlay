// Uploads the browser's own log lines and saved "Report a problem" reports to the PC over the
// authenticated session control channel (wire protocol: docs/architecture/client-log-upload.md,
// capability client-logs-v1). The session never depends on it: every failure is caught here and
// only logged locally. The host's logs-state is the source of truth for where to resume; nothing
// about cursors is persisted by the browser.
//
// Priority (the stream comes first): a message goes out only when the ctl send buffer is at or
// below 16 KiB, no input was sent in the last 250 ms, and the minimum batch interval passed.
// One log batch and one report are in flight at a time.

export const LOGS_CAP = "client-logs-v1";
export const LOG_STREAM = "browser";
export const LIMIT_DEFAULTS = Object.freeze({
  maxBatchBytes: 16_384, maxBatchLines: 100, connectionBudgetBytes: 8_388_608, minBatchIntervalMs: 500,
});
export const MAX_BUFFERED = 16_384;
export const INPUT_QUIET_MS = 250;
export const ACK_TIMEOUT_MS = 20_000;
const ENVELOPE_BYTES = 256;
const MAX_CONSECUTIVE_REJECTS = 5;
const MAX_RETRY_AFTER_MS = 60_000;
const BUFFER_POLL_MS = 250;

/** A fixed-width, byte-wise comparable cursor for BPLog's numeric line cursor. */
export const formatCursor = n => String(Math.max(0, Math.floor(Number(n) || 0))).padStart(20, "0");
export const parseCursor = text => (typeof text === "string" && /^[0-9]{1,20}$/.test(text) ? Number(text) : null);

const encoder = typeof TextEncoder === "function" ? new TextEncoder() : null;
const utf8Length = text => (encoder ? encoder.encode(text).length : text.length * 3);

function positiveOr(value, fallback, ceiling = Infinity) {
  return Number.isFinite(value) && value > 0 ? Math.min(value, ceiling) : fallback;
}

/**
 * `send(message)` returns true when the message was accepted for sending. `bufferedAmount()` is
 * the socket's send buffer, `lastInputAt()` the clock time of the last input message,
 * `isActive()` false once the session is gone. `now`, `schedule` and `cancel` are test seams.
 */
export function createLogUploader({
  log = globalThis.BPLog, send, bufferedAmount = () => 0, lastInputAt = () => -Infinity, isActive = () => true,
  pc = null, now = () => Date.now(),
  schedule = (fn, ms) => setTimeout(fn, ms), cancel = handle => clearTimeout(handle),
} = {}) {
  let started = false, stopped = false, timer = null, timerAt = Infinity, unsubscribe = null;
  let limits = { ...LIMIT_DEFAULTS };
  let position = null, historyEnd = 0, scannedTo = 0, nextId = 1;
  let inflight = null, retryBatch = null, lastSend = -Infinity, notBefore = 0;
  let budgetUsed = 0, budgetStopped = false, rejects = 0, skipped = 0;
  let reportQueue = [], reportInflight = null, loadingReports = false, reloadReports = false;
  const tried = new Set();
  const stats = { batches: 0, lines: 0, reports: 0 };

  const note = (level, msg, fields) => { try { log?.log?.(level, "dk-upload", msg, fields); } catch { /* ignore */ } };
  const describe = error => String(error?.message ?? error).slice(0, 160);

  function arm(delay) {
    if (stopped) return;
    const at = now() + Math.max(0, delay);
    if (timer !== null) {
      if (timerAt <= at) return;
      cancel(timer);
    }
    timerAt = at;
    timer = schedule(() => { timer = null; timerAt = Infinity; tick(); }, Math.max(0, delay));
  }

  /** 0 when a message may go out now, else how long to wait. */
  function gateDelay(t) {
    let wait = 0;
    if (bufferedAmount() > MAX_BUFFERED) wait = Math.max(wait, BUFFER_POLL_MS);
    wait = Math.max(wait, lastInputAt() + INPUT_QUIET_MS - t);
    wait = Math.max(wait, lastSend + limits.minBatchIntervalMs - t);
    wait = Math.max(wait, notBefore - t);
    return wait > 0 ? Math.ceil(wait) : 0;
  }

  function utf8Message(message) { return utf8Length(JSON.stringify(message)); }

  function emit(message) {
    let accepted = false;
    try { accepted = send(message) === true; } catch (error) { note("warn", "upload send failed", { cause: describe(error) }); }
    lastSend = now();
    if (accepted) budgetUsed += utf8Message(message);
    return accepted;
  }

  function buildBatch() {
    const head = log?.head?.() ?? 0;
    if (head <= position || head <= scannedTo) return null;
    const items = log.drain(pc, position, limits.maxBatchLines);
    if (!items.length) { scannedTo = head; return null; }
    const live = items[0].cursor > historyEnd;
    const pool = live ? items : items.filter(item => item.cursor <= historyEnd);
    const lines = [];
    let bytes = ENVELOPE_BYTES, last = null;
    for (const item of pool) {
      const line = JSON.stringify(item.line);
      const cost = utf8Length(JSON.stringify(line)) + 1;
      if (cost + ENVELOPE_BYTES > limits.maxBatchBytes) {
        if (lines.length) break;
        position = item.cursor; skipped++;
        continue;
      }
      if (bytes + cost > limits.maxBatchBytes) break;
      bytes += cost; lines.push(line); last = item;
    }
    if (!lines.length) return null;
    return { from: formatCursor(position), to: formatCursor(last.cursor), toNum: last.cursor, live, lines };
  }

  function sendLogs(t) {
    const batch = retryBatch ?? buildBatch();
    if (!batch) return false;
    const message = { type: "logs-batch", v: 1, stream: LOG_STREAM, id: nextId, from: batch.from, to: batch.to, live: batch.live, lines: batch.lines };
    if (budgetUsed + utf8Message(message) > limits.connectionBudgetBytes) {
      budgetStopped = true; retryBatch = null;
      note("info", "log upload stopped: connection budget used", { used: budgetUsed });
      return false;
    }
    nextId++;
    retryBatch = null;
    if (!emit(message)) { retryBatch = batch; notBefore = t + Math.max(limits.minBatchIntervalMs, 1000); return true; }
    inflight = { id: message.id, batch, sentAt: now() };
    stats.batches++;
    return true;
  }

  function sendReport() {
    const report = reportQueue[0];
    if (!report || typeof report.id !== "string") { reportQueue.shift(); return true; }
    const body = "screenshot" in report ? report : { ...report, screenshot: null };
    const message = { type: "feedback-begin", v: 1, id: report.id, report: body, pngBytes: 0, chunks: 0 };
    reportQueue.shift();
    tried.add(report.id); // one attempt per connection; a failed one waits for the next
    if (!emit(message)) return true;
    reportInflight = { id: report.id, sentAt: now() };
    return true;
  }

  function loadReports() {
    if (stopped || typeof log?.listReports !== "function") return;
    if (loadingReports) { reloadReports = true; return; }
    loadingReports = true;
    Promise.resolve().then(() => log.listReports()).then(list => {
      loadingReports = false;
      if (stopped) return;
      if (reloadReports) { reloadReports = false; loadReports(); }
      const queued = new Set(reportQueue.map(report => report?.id));
      for (const report of Array.isArray(list) ? list : []) {
        if (!report || typeof report.id !== "string" || tried.has(report.id) || queued.has(report.id) || report.id === reportInflight?.id) continue;
        queued.add(report.id);
        reportQueue.push(report);
      }
      reportQueue.sort((a, b) => String(a.created ?? "").localeCompare(String(b.created ?? "")));
      arm(0);
    }).catch(error => { loadingReports = false; note("warn", "saved reports could not be read", { cause: describe(error) }); });
  }

  function tick() {
    if (stopped) return;
    try {
      if (!isActive()) return stop();
      const t = now();
      if (inflight && t - inflight.sentAt > ACK_TIMEOUT_MS) { retryBatch = inflight.batch; inflight = null; }
      if (reportInflight && t - reportInflight.sentAt > ACK_TIMEOUT_MS) reportInflight = null;
      const wait = gateDelay(t);
      if (wait > 0) return arm(wait);
      let sent = false;
      if (reportQueue.length && !reportInflight) sent = sendReport();
      else if (position !== null && !inflight && !budgetStopped && rejects < MAX_CONSECUTIVE_REJECTS) sent = sendLogs(t);
      arm(sent ? limits.minBatchIntervalMs : Math.max(limits.minBatchIntervalMs, 250));
    } catch (error) {
      note("warn", "upload tick failed", { cause: describe(error) });
      try { arm(5000); } catch { /* the uploader is done */ }
    }
  }

  function onState(message) {
    if (position !== null) return; // once per connection; the host's first answer is the truth
    const given = message.limits && typeof message.limits === "object" ? message.limits : {};
    limits = {
      maxBatchBytes: positiveOr(given.maxBatchBytes, LIMIT_DEFAULTS.maxBatchBytes, LIMIT_DEFAULTS.maxBatchBytes),
      maxBatchLines: Math.floor(positiveOr(given.maxBatchLines, LIMIT_DEFAULTS.maxBatchLines, LIMIT_DEFAULTS.maxBatchLines)),
      connectionBudgetBytes: positiveOr(given.connectionBudgetBytes, LIMIT_DEFAULTS.connectionBudgetBytes),
      minBatchIntervalMs: Number.isFinite(given.minBatchIntervalMs) && given.minBatchIntervalMs >= 0
        ? Math.min(given.minBatchIntervalMs, MAX_RETRY_AFTER_MS) : LIMIT_DEFAULTS.minBatchIntervalMs,
    };
    const cursors = message.cursors && typeof message.cursors === "object" ? message.cursors : {};
    position = parseCursor(cursors[LOG_STREAM]) ?? 0;
    historyEnd = log?.head?.() ?? 0;
    scannedTo = 0;
    arm(0);
  }

  function onLogsAck(message) {
    if (message.stream !== LOG_STREAM || !inflight || message.id !== inflight.id) return;
    const { batch } = inflight;
    inflight = null;
    const retryAfter = Number.isFinite(message.retryAfterMs) ? Math.min(Math.max(message.retryAfterMs, 0), MAX_RETRY_AFTER_MS) : 0;
    switch (message.status) {
      case "ok": {
        const stored = parseCursor(message.cursor);
        position = stored !== null && stored >= batch.toNum ? stored : batch.toNum;
        rejects = 0;
        stats.lines += batch.lines.length;
        if (Number(message.dropped) > 0) note("info", "host dropped upload lines", { dropped: Number(message.dropped) });
        break;
      }
      case "rate-limited":
        retryBatch = batch;
        notBefore = now() + Math.max(retryAfter, limits.minBatchIntervalMs);
        break;
      case "budget":
        budgetStopped = true;
        note("info", "log upload stopped: host budget used");
        break;
      case "rejected":
        position = batch.toNum; // do not retry this batch
        rejects++;
        note("warn", "host rejected a log batch", { reason: String(message.reason ?? "").slice(0, 80), lines: batch.lines.length });
        break;
      default:
        retryBatch = batch;
        notBefore = now() + 5000;
    }
    arm(0);
  }

  function onFeedbackAck(message) {
    if (!reportInflight || message.id !== reportInflight.id) return;
    const { id } = reportInflight;
    reportInflight = null;
    if (message.status === "ok") {
      stats.reports++;
      Promise.resolve().then(() => log.deleteReport(id)).then(() => { try { log.reportSent?.(id); } catch { /* ignore */ } })
        .catch(error => note("warn", "sent report could not be removed", { cause: describe(error) }));
    } else {
      note("info", "report not accepted, it stays for the next connection", { status: String(message.status).slice(0, 16), reason: String(message.reason ?? "").slice(0, 80) });
    }
    arm(0);
  }

  return {
    start() {
      if (started || stopped) return;
      started = true;
      try { unsubscribe = log?.onReportSaved?.(() => loadReports()) ?? null; } catch { unsubscribe = null; }
      loadReports();
      arm(0);
    },
    /** Feeds a decoded host message in. Unknown versions and types are ignored. */
    handle(message) {
      if (stopped || !message || typeof message !== "object" || message.v !== 1) return false;
      try {
        if (message.type === "logs-state") onState(message);
        else if (message.type === "logs-ack") onLogsAck(message);
        else if (message.type === "feedback-ack") onFeedbackAck(message);
        else return false;
      } catch (error) { note("warn", "upload message failed", { cause: describe(error) }); }
      return true;
    },
    stop,
    stats: () => ({ ...stats, position, skipped, budgetUsed, budgetStopped, inflight: Boolean(inflight), queuedReports: reportQueue.length }),
  };

  function stop() {
    if (stopped) return;
    stopped = true;
    try { if (timer !== null) cancel(timer); } catch { /* ignore */ }
    timer = null;
    try { unsubscribe?.(); } catch { /* ignore */ }
    unsubscribe = null;
  }
}
