// Browser viewer logging. A classic script (not a module) so it can run before everything else
// and catch errors from the first line of the page. It defines globalThis.BPLog.
//
// Lines follow the shared JSONL schema (docs/logging.md): {ts, level, src:"browser", device, pc,
// session, target, msg, fields}. Every string passes through redact() BEFORE it is kept; a line
// that is stored has never held a secret. Lines live in memory and, when IndexedDB works, in the
// database "bp-logs" (store "lines", ring capped at 2 MB, oldest evicted first). Without
// IndexedDB the in-memory ring is all there is. Reports from "Report a problem" go to the
// store "feedback" in the same database.
//
// Nothing here ever throws into a caller. dk-upload.js uploads lines to the PC over the encrypted
// session channel with BPLog.drain(pc, afterCursor); the host acks by cursor.
(function installBPLog(root) {
  "use strict";

  const SRC = "browser";
  const DB_NAME = "bp-logs";
  const DB_VERSION = 1;
  const LINES = "lines";
  const FEEDBACK = "feedback";
  const DEFAULT_CAP_BYTES = 2 * 1024 * 1024;
  const MAX_LINE = 8 * 1024;
  const MAX_MSG = 4096;
  const MAX_FIELD = 2048;
  const MAX_STACK = 4096;
  const MAX_PENDING = 2000;
  const FLUSH_MS = 250;
  const LEVELS = { trace: 0, debug: 1, info: 2, warn: 3, error: 4 };
  const REDACTED = "[redacted]";
  const BLOB = "[redacted-blob]";

  // ---- redaction (spec section 2, rules 1-6; rule 7 is the try/catch wrappers) ------------
  const SENSITIVE_WORDS = ["password", "passphrase", "passwd", "secret", "token", "key", "proof", "ticket",
    "code", "pairing", "psk", "signature", "auth", "cookie", "credential"];
  // Substrings that contain a sensitive word but are not secrets. keyboard, keyframe, keycode and
  // keyid are the spec's list; codec, decod and encod keep "codec", "decoder" and "encode" readable.
  const ALLOWED_PARTS = ["keyboard", "keyframe", "keycode", "keyid", "codec", "decod", "encod"];
  const URL_PARAMS = "token|key|secret|password|passphrase|code|ticket|proof|pairing|sig";
  const URL_PARAM_RE = new RegExp(`([?&#;])(${URL_PARAMS})=([^&#\\s"'<>]*)`, "gi");
  const BEARER_RE = /\bBearer\s+[^\s"',;]+/gi;
  const AUTH_HEADER_RE = /^[ \t]*Authorization[ \t]*:.*$/gim;
  const KV_RE = /(?<![A-Za-z0-9_.-])(["']?)([A-Za-z_][A-Za-z0-9_.-]{0,63})\1([ \t]*[:=][ \t]*)("(?:[^"\\\r\n]|\\.)*"|'(?:[^'\\\r\n]|\\.)*'|[^\s]*)/g;
  const BLOB_RE = /[A-Za-z0-9+/_=-]{40,}/g;
  const UNTOUCHED_KEYS = new Set(["device", "pc", "session"]);

  /** Why a key marks its value secret: null, "code" (a count or status may pass) or "secret". */
  function sensitiveKind(key) {
    let compact = String(key).replace(/([a-z0-9])([A-Z])/g, "$1 $2").toLowerCase().replace(/[^a-z0-9]+/g, "");
    for (const part of ALLOWED_PARTS) compact = compact.split(part).join("");
    let kind = null;
    for (const word of SENSITIVE_WORDS) {
      if (!compact.includes(word)) continue;
      if (word !== "code") return "secret";
      kind = "code";
    }
    return kind;
  }

  function replaceKeyValues(text) {
    const re = new RegExp(KV_RE.source, "g");
    let out = "", last = 0, match;
    while ((match = re.exec(text)) !== null) {
      const [whole, quote, key, sep, value] = match;
      if (whole === "") { re.lastIndex++; continue; }
      const kind = sensitiveKind(key);
      if (!kind || (kind === "code" && /^\d{1,5}$/.test(value))) continue;
      const first = value[0];
      const quoted = first === '"' || first === "'";
      // A quoted value ends at its quote and a bare value after "=" at a space (logfmt). A bare
      // value after ":" runs to the end of the line, since prose such as a passphrase has spaces.
      let end = match.index + whole.length;
      if (!quoted && !sep.includes("=")) {
        const eol = text.slice(end).search(/[\r\n]/);
        end = eol < 0 ? text.length : end + eol;
      }
      out += text.slice(last, match.index) + quote + key + quote + sep + (quoted ? `${first}${REDACTED}${first}` : REDACTED);
      last = end;
      re.lastIndex = end;
    }
    return out + text.slice(last);
  }

  function createRedactor() {
    const literals = [];
    function registerSecret(value) {
      if (typeof value !== "string" || value === "" || literals.includes(value)) return;
      literals.push(value);
      literals.sort((a, b) => b.length - a.length);
    }
    function redactText(input, { blobs = true } = {}) {
      let text = String(input);
      for (const literal of literals) text = text.split(literal).join(REDACTED);
      text = text.replace(AUTH_HEADER_RE, `Authorization: ${REDACTED}`);
      text = text.replace(BEARER_RE, `Bearer ${REDACTED}`);
      text = text.replace(URL_PARAM_RE, (_all, lead, name) => `${lead}${name}=${REDACTED}`);
      text = replaceKeyValues(text);
      if (blobs) text = text.replace(BLOB_RE, BLOB);
      return text;
    }
    function redactValue(value, key, depth) {
      if (value === null || value === undefined || typeof value === "boolean") return value ?? null;
      if (typeof key === "string") {
        const kind = sensitiveKind(key);
        if (kind && !(kind === "code" && typeof value === "number" && Number.isFinite(value)))
          return REDACTED;
      }
      if (typeof value === "number") return Number.isFinite(value) ? value : String(value);
      if (typeof value === "bigint") return String(value);
      if (typeof value === "string") {
        return redactText(value, { blobs: !UNTOUCHED_KEYS.has(key) });
      }
      if (typeof value === "function" || typeof value === "symbol") return `[${typeof value}]`;
      if (depth >= 4) return "[nested]";
      if (Array.isArray(value)) return value.slice(0, 32).map(item => redactValue(item, undefined, depth + 1));
      if (value instanceof Error) {
        return { error: redactText(value.message ?? value), stack: redactText(String(value.stack ?? "").slice(0, MAX_STACK)) };
      }
      const out = {};
      let count = 0;
      for (const name of Object.keys(value)) {
        if (++count > 48) break;
        out[name] = redactValue(value[name], name, depth + 1);
      }
      return out;
    }
    /** Redacts one string (msg, report text). Never throws. */
    function redact(text) {
      try { return redactText(text); } catch { return "[redaction failed]"; }
    }
    /** Redacts a fields object (or any value) by key and by content. Never throws. */
    function redactFields(fields) {
      try { return redactValue(fields, undefined, 0); } catch { return {}; }
    }
    return { registerSecret, redact, redactFields, redactText };
  }

  // ---- helpers ---------------------------------------------------------------------------
  function clip(text, max) {
    text = String(text);
    return text.length > max ? `${text.slice(0, max)}...[truncated]` : text;
  }

  function idOrNull(value) {
    if (typeof value !== "string") return null;
    const text = value.replace(/[^\x20-\x7e]/g, "").slice(0, 64);
    return text || null;
  }

  function isoTime(ms) {
    return new Date(ms).toISOString();
  }

  function formatArgs(args) {
    return args.map(arg => {
      if (typeof arg === "string") return arg;
      if (arg instanceof Error) return `${arg.name}: ${arg.message}`;
      try { return JSON.stringify(arg) ?? String(arg); } catch { return String(arg); }
    }).join(" ");
  }

  /** The logger. `env` carries the platform pieces so tests can supply their own. */
  function createBPLog(env = {}) {
    const now = env.now ?? (() => Date.now());
    const schedule = env.setTimeout ?? ((fn, ms) => setTimeout(fn, ms));
    const capBytes = env.capBytes ?? DEFAULT_CAP_BYTES;
    const idb = "indexedDB" in env ? env.indexedDB : root.indexedDB;
    const redactor = createRedactor();
    const context = { device: null, pc: null, session: null };
    const extra = {};

    let ring = [];                 // {cursor, bytes, record}, ascending cursor
    let ringBytes = 0;
    let lastCursor = 0;
    let db = null;                 // IDBDatabase once open, else null (memory only)
    let dbState = idb ? "opening" : "memory";
    let pendingAdds = [];          // {cursor, record}
    let pendingDeletes = [];
    let flushTimer = null;
    let dropped = 0;
    let lastDropNote = 0;
    let inWrite = false;
    const memReports = [];
    const reportListeners = { saved: new Set(), sent: new Set() };
    let capturing = false;

    const nextCursor = () => {
      // Time-based so cursors keep rising across reloads and tabs without a counter store.
      lastCursor = Math.max(lastCursor + 1, now() * 1000);
      return lastCursor;
    };

    function trimRing() {
      let cut = 0;
      while (ringBytes > capBytes && cut < ring.length - 1) {
        ringBytes -= ring[cut].bytes;
        pendingDeletes.push(ring[cut].cursor);
        cut++;
      }
      if (cut) ring = ring.slice(cut);
    }

    function buildRecord(level, target, msg, fields) {
      let text = redactor.redact(clip(msg, MAX_MSG));
      let clean = fields === undefined || fields === null ? {} : redactor.redactFields(fields);
      if (clean === null || typeof clean !== "object" || Array.isArray(clean)) clean = { value: clean };
      for (const name of Object.keys(clean)) {
        if (typeof clean[name] === "string") clean[name] = clip(clean[name], name === "stack" ? MAX_STACK : MAX_FIELD);
      }
      let record = {
        ts: isoTime(now()), level, src: SRC, device: context.device, pc: context.pc, session: context.session,
        target: clip(redactor.redact(String(target ?? "")), 128), msg: text, fields: clean,
      };
      let size = JSON.stringify(record).length;
      if (size > MAX_LINE) {
        record = { ...record, fields: { truncated: true } };
        size = JSON.stringify(record).length;
        if (size > MAX_LINE) { record.msg = clip(record.msg, 512); size = JSON.stringify(record).length; }
      }
      return { record, size };
    }

    function write(level, target, msg, fields) {
      if (inWrite) return null;
      inWrite = true;
      try {
        if (LEVELS[level] === undefined || LEVELS[level] < LEVELS.info) return null;
        let built;
        try { built = buildRecord(level, target, msg, fields); }
        catch {
          built = { record: { ts: isoTime(now()), level, src: SRC, device: context.device, pc: context.pc,
            session: context.session, target: "log", msg: "[redaction failed]", fields: {} }, size: 200 };
        }
        const cursor = nextCursor();
        ring.push({ cursor, bytes: built.size, record: built.record });
        ringBytes += built.size;
        trimRing();
        if (dbState !== "memory") {
          pendingAdds.push({ cursor, record: built.record });
          if (pendingAdds.length > MAX_PENDING) { pendingAdds.shift(); dropped++; }
          scheduleFlush();
        }
        return cursor;
      } catch {
        return null;
      } finally {
        inWrite = false;
        noteDrops();
      }
    }

    function noteDrops() {
      if (!dropped || inWrite) return;
      const t = now();
      if (t - lastDropNote < 60_000) return;
      lastDropNote = t;
      const count = dropped;
      dropped = 0;
      write("warn", "log", "log lines were not saved to the browser store", { dropped: count });
    }

    // ---- IndexedDB persistence (write-behind, best effort) --------------------------------
    function scheduleFlush() {
      if (flushTimer !== null || !db) return;
      flushTimer = schedule(() => { flushTimer = null; flush(); }, FLUSH_MS);
    }

    function failDb() {
      dbState = "memory";
      try { db?.close(); } catch { /* already gone */ }
      db = null;
      pendingAdds = [];
      pendingDeletes = [];
    }

    function flush() {
      if (!db || (!pendingAdds.length && !pendingDeletes.length)) return;
      const adds = pendingAdds, deletes = pendingDeletes;
      pendingAdds = []; pendingDeletes = [];
      try {
        const tx = db.transaction(LINES, "readwrite");
        const store = tx.objectStore(LINES);
        for (const item of adds) store.put(item.record, item.cursor);
        for (const cursor of deletes) store.delete(cursor);
        tx.onerror = tx.onabort = () => failDb();
      } catch {
        failDb();
      }
    }

    function loadStored(database) {
      try {
        const tx = database.transaction(LINES, "readonly");
        const request = tx.objectStore(LINES).openCursor();
        const found = [];
        request.onsuccess = () => {
          const cursor = request.result;
          if (cursor) {
            found.push({ cursor: cursor.key, record: cursor.value });
            cursor.continue();
            return;
          }
          mergeStored(found);
        };
        request.onerror = () => failDb();
      } catch {
        failDb();
      }
    }

    function mergeStored(found) {
      try {
        const have = new Set(ring.map(item => item.cursor));
        const merged = ring.slice();
        for (const item of found) {
          if (have.has(item.cursor) || !item.record || typeof item.record !== "object") continue;
          merged.push({ cursor: item.cursor, bytes: JSON.stringify(item.record).length, record: item.record });
        }
        merged.sort((a, b) => a.cursor - b.cursor);
        ring = merged;
        ringBytes = ring.reduce((sum, item) => sum + item.bytes, 0);
        for (const item of ring) if (item.cursor > lastCursor) lastCursor = item.cursor;
        trimRing();
        dbState = "ready";
        scheduleFlush();
      } catch {
        failDb();
      }
    }

    function openDb() {
      if (!idb) return;
      try {
        const request = idb.open(DB_NAME, DB_VERSION);
        request.onupgradeneeded = () => {
          const database = request.result;
          if (!database.objectStoreNames.contains(LINES)) {
            const store = database.createObjectStore(LINES, { autoIncrement: true });
            store.createIndex("pc", "pc", { unique: false });
          }
          if (!database.objectStoreNames.contains(FEEDBACK)) database.createObjectStore(FEEDBACK, { keyPath: "id" });
        };
        request.onsuccess = () => {
          db = request.result;
          db.onversionchange = () => { try { db.close(); } catch { /* ignore */ } db = null; dbState = "memory"; };
          loadStored(db);
        };
        request.onerror = request.onblocked = () => failDb();
      } catch {
        failDb();
      }
    }

    // ---- feedback store -------------------------------------------------------------------
    /** Saves a report. Resolves {ok, persisted}; persisted is false when only memory holds it. */
    function saveReport(report) {
      return new Promise(resolve => {
        const done = result => { resolve(result); notify("saved", report?.id); };
        const keep = () => { memReports.push(report); done({ ok: true, persisted: false }); };
        try {
          if (!db) return keep();
          const tx = db.transaction(FEEDBACK, "readwrite");
          tx.objectStore(FEEDBACK).put(report);
          tx.oncomplete = () => done({ ok: true, persisted: true });
          tx.onerror = tx.onabort = () => keep();
        } catch {
          keep();
        }
      });
    }

    /** Removes a report once the PC has stored it. Resolves true when it is gone. */
    function deleteReport(id) {
      return new Promise(resolve => {
        const memory = () => {
          for (let i = memReports.length - 1; i >= 0; i--) if (memReports[i]?.id === id) memReports.splice(i, 1);
        };
        memory();
        try {
          if (!db) return resolve(true);
          const tx = db.transaction(FEEDBACK, "readwrite");
          tx.objectStore(FEEDBACK).delete(id);
          tx.oncomplete = () => resolve(true);
          tx.onerror = tx.onabort = () => resolve(false);
        } catch {
          resolve(false);
        }
      });
    }

    function notify(kind, id) {
      for (const listener of [...reportListeners[kind]]) {
        try { listener(id); } catch { /* a listener never breaks the logger */ }
      }
    }
    function listen(kind, listener) {
      if (typeof listener !== "function") return () => {};
      reportListeners[kind].add(listener);
      return () => { reportListeners[kind].delete(listener); };
    }

    /** Saved reports waiting to go to a PC (phase B). */
    function listReports() {
      return new Promise(resolve => {
        try {
          if (!db) return resolve(memReports.slice());
          const request = db.transaction(FEEDBACK, "readonly").objectStore(FEEDBACK).getAll();
          request.onsuccess = () => resolve([...(request.result || []), ...memReports]);
          request.onerror = () => resolve(memReports.slice());
        } catch {
          resolve(memReports.slice());
        }
      });
    }

    // ---- capture --------------------------------------------------------------------------
    function errorFields(error, extraFields) {
      const fields = { ...extraFields };
      if (error && typeof error === "object") {
        if (error.name) fields.name = String(error.name);
        if (error.stack) fields.stack = String(error.stack);
        fields.error = String(error.message ?? error);
      } else if (error !== undefined) {
        fields.error = String(error);
      }
      return fields;
    }

    function install() {
      const target = env.target ?? root;
      try {
        target.addEventListener?.("error", event => {
          const fields = errorFields(event?.error, { source: String(event?.filename || ""), line: event?.lineno || 0, column: event?.colno || 0 });
          if (!fields.error) fields.error = String(event?.message || "unknown");
          write("error", "uncaught", String(event?.message || fields.error), fields);
        });
        target.addEventListener?.("unhandledrejection", event => {
          const reason = event?.reason;
          const fields = errorFields(reason, {});
          if (!fields.error) fields.error = "unknown";
          write("error", "uncaught", `Unhandled rejection: ${fields.error}`, fields);
        });
        target.addEventListener?.("pagehide", () => flush());
        target.addEventListener?.("visibilitychange", () => { if (env.document?.visibilityState === "hidden") flush(); });
      } catch { /* a restricted page keeps memory logging */ }
      const sink = env.console ?? root.console;
      for (const level of ["warn", "error"]) {
        const original = sink?.[level];
        if (typeof original !== "function") continue;
        sink[level] = function mirroredConsole(...args) {
          try { original.apply(this, args); } catch { /* console gone */ }
          if (capturing) return;
          capturing = true;
          try { write(level, "console", formatArgs(args), {}); } catch { /* never throw */ } finally { capturing = false; }
        };
      }
    }

    // ---- public API -----------------------------------------------------------------------
    const api = {
      info: (target, msg, fields) => write("info", target, msg, fields),
      warn: (target, msg, fields) => write("warn", target, msg, fields),
      error: (target, msg, fields) => write("error", target, msg, fields),
      log: (level, target, msg, fields) => write(level, target, msg, fields),
      setContext(next = {}) {
        try {
          for (const name of ["device", "pc", "session"]) {
            if (name in next) context[name] = idOrNull(next[name]);
          }
          if ("build" in next) extra.build = idOrNull(next.build);
        } catch { /* ignore */ }
      },
      context: () => ({ ...context, ...extra }),
      /** Registers a literal secret (a passphrase, a code) so it is replaced wherever it appears. */
      registerSecret: value => redactor.registerSecret(value),
      redact: text => redactor.redact(text),
      redactFields: fields => redactor.redactFields(fields),
      recent(n = 200) {
        try { return ring.slice(-Math.max(0, n | 0)).map(item => item.record); } catch { return []; }
      },
      errors(n = 20) {
        try { return ring.filter(item => item.record.level === "error").slice(-Math.max(0, n | 0)).map(item => item.record); }
        catch { return []; }
      },
      /** Phase B hook: lines for this PC (or tied to no PC) after the cursor, oldest first. */
      drain(pc, afterCursor = 0, limit = 500) {
        try {
          const after = Number.isFinite(afterCursor) ? afterCursor : 0;
          const out = [];
          for (const item of ring) {
            if (item.cursor <= after) continue;
            if (item.record.pc !== null && item.record.pc !== pc) continue;
            out.push({ cursor: item.cursor, line: item.record });
            if (out.length >= limit) break;
          }
          return out;
        } catch { return []; }
      },
      saveReport,
      listReports,
      deleteReport,
      /** Calls the listener with the report id after a report is saved (the uploader sends it). */
      onReportSaved: listener => listen("saved", listener),
      /** Calls the listener with the report id after the PC acknowledged it. */
      onReportSent: listener => listen("sent", listener),
      reportSent: id => notify("sent", id),
      /** Cursor of the newest line written so far (0 when none). */
      head: () => lastCursor,
      flush,
      install,
      /** "memory" without IndexedDB, "opening" until the store loaded, then "ready". */
      state: () => dbState,
      stats: () => ({ lines: ring.length, bytes: ringBytes, capBytes, state: dbState }),
    };
    openDb();
    return api;
  }

  root.BPLogCreate = createBPLog;
  if (!root.BPLog) {
    try {
      const log = createBPLog({ document: root.document });
      log.install();
      root.BPLog = log;
    } catch { /* the viewer works without logging */ }
  }
})(globalThis);
