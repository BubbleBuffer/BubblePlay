import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";
import { createLogUploader, formatCursor, parseCursor, LOGS_CAP, MAX_FEEDBACK_BYTES } from "../public/dk-upload.js";
import { createFeedback, SAVED_MEMORY_TEXT, SENT_TEXT } from "../public/dk-feedback.js";

const contract = JSON.parse(readFileSync(new URL("../../fixtures/client-log-upload-contract.json", import.meta.url), "utf8"));
const fixture = name => contract.messages.find(message => message.name === name).json;
const SOURCE = readFileSync(new URL("../public/dk-log.js", import.meta.url), "utf8");
const PC = "pc-1";
const tick = async () => { for (let i = 0; i < 6; i++) await new Promise(resolve => setImmediate(resolve)); };

// ---- the real logger in its own context (memory only) ---------------------------------------
function makeLog(clock) {
  const context = vm.createContext({ console: { warn() {}, error() {} }, addEventListener() {}, setTimeout: () => 1,
    Date, JSON, Error, Promise, Set, Math, Number, String, Array, Object, RegExp, indexedDB: undefined });
  context.globalThis = context;
  vm.runInContext(SOURCE, context);
  const raw = context.BPLogCreate({ indexedDB: undefined, setTimeout: () => 1, now: clock, console: { warn() {}, error() {} } });
  const flat = value => JSON.parse(JSON.stringify(value));
  return {
    raw,
    log: {
      log: (...a) => raw.log(...a), info: (...a) => raw.info(...a), warn: (...a) => raw.warn(...a),
      setContext: c => raw.setContext(c), head: () => raw.head(),
      drain: (pc, after, limit) => flat(raw.drain(pc, after, limit)),
      listReports: async () => flat(await raw.listReports()),
      saveReport: report => raw.saveReport(report), deleteReport: id => raw.deleteReport(id),
      onReportSaved: l => raw.onReportSaved(l), onReportSent: l => raw.onReportSent(l), reportSent: id => raw.reportSent(id),
      redact: t => raw.redact(t), recent: n => flat(raw.recent(n)), errors: n => flat(raw.errors(n)), context: () => flat(raw.context()),
    },
  };
}

function rig(options = {}) {
  let t = 1_000_000;
  const clock = () => t;
  const { raw, log } = options.log ?? makeLog(clock);
  const timers = [];
  const sent = [];
  const state = { buffered: 0, lastInput: -Infinity, active: true, accept: true, throwOnSend: false };
  const uploader = createLogUploader({
    log, pc: PC, now: clock,
    send: message => { if (state.throwOnSend) throw new Error("socket gone"); if (state.accept) sent.push(JSON.parse(JSON.stringify(message))); return state.accept; },
    bufferedAmount: () => state.buffered, lastInputAt: () => state.lastInput, isActive: () => state.active,
    schedule: (fn, ms) => { const timer = { at: t + ms, fn }; timers.push(timer); return timer; },
    cancel: timer => { const i = timers.indexOf(timer); if (i >= 0) timers.splice(i, 1); },
  });
  // Runs timers up to `ms` later, in time order.
  const advance = async (ms) => {
    const end = t + ms;
    for (;;) {
      timers.sort((a, b) => a.at - b.at);
      const next = timers[0];
      if (!next || next.at > end) break;
      timers.shift();
      t = Math.max(t, next.at);
      next.fn();
      await tick();
    }
    t = end;
  };
  const lines = n => { for (let i = 0; i < n; i++) log.info("test", `line ${i}`); };
  const batches = () => sent.filter(m => m.type === "logs-batch");
  const ack = (batch, extra = {}) => uploader.handle({ type: "logs-ack", v: 1, stream: "browser", id: batch.id, status: "ok",
    cursor: batch.to, dropped: 0, retryAfterMs: 0, ...extra });
  const state0 = (cursors = {}, limits = {}) => uploader.handle({ type: "logs-state", v: 1, cursors,
    limits: { ...contract.limits, ...limits } });
  return { t: () => t, clock, raw, log, uploader, sent, state, timers, advance, lines, batches, ack, state0, setT: v => { t = v; } };
}

const lineMsgs = batch => batch.lines.map(line => JSON.parse(line).msg);

// ---- contract fixtures ----------------------------------------------------------------------

test("cursors are fixed-width, byte-wise comparable strings", () => {
  assert.equal(formatCursor(42), "00000000000000000042");
  assert.equal(formatCursor(1_800_000_000_000_000).length, 20);
  assert.ok(formatCursor(9) < formatCursor(10));
  assert.ok(formatCursor(1_800_000_000_000_001) > formatCursor(1_799_999_999_999_999));
  assert.equal(parseCursor(formatCursor(77)), 77);
  assert.equal(parseCursor("2026-11-02T13:45:12.345Z#000042"), null);
  assert.equal(LOGS_CAP, contract.capability);
});

test("every fixture message: host messages are handled, client messages match the fixture shape", async () => {
  const r = rig();
  r.lines(3);
  assert.equal(r.state0(fixture("logs-state").cursors, fixture("logs-state").limits), true, "logs-state is accepted");
  await r.advance(0);
  const [batch] = r.batches();
  const wire = fixture("logs-batch");
  assert.deepEqual(Object.keys(batch).sort(), Object.keys(wire).sort(), "logs-batch has the fixture's fields");
  assert.equal(batch.v, 1);
  assert.equal(batch.stream, "browser");
  assert.equal(batch.from, formatCursor(0), "a stream the host never saw starts from the beginning");
  assert.match(batch.to, /^[0-9]{20}$/);
  assert.ok(batch.to > batch.from);
  assert.equal(batch.live, false);
  for (const line of batch.lines) assert.equal(typeof line, "string"), JSON.parse(line);

  // acks with the fixture ids match no batch in flight and change nothing
  for (const name of ["logs-ack-ok", "logs-ack-rate-limited", "logs-ack-budget", "logs-ack-rejected"]) {
    assert.equal(r.uploader.handle({ ...fixture(name), stream: "browser" }), true, name);
  }
  assert.equal(r.uploader.stats().inflight, true);
  assert.equal(r.uploader.stats().budgetStopped, false);
  assert.equal(r.uploader.handle({ ...fixture("logs-state"), v: 2 }), false, "unknown versions are ignored");
  assert.equal(r.uploader.handle({ type: "bogus", v: 1 }), false);

  // feedback fixtures
  r.uploader.start();
  const report = { ...fixture("feedback-begin").report, id: "rep-1" };
  await r.log.saveReport(report);
  await tick();
  await r.advance(2000);
  const begin = r.sent.find(m => m.type === "feedback-begin");
  assert.ok(begin);
  assert.deepEqual(Object.keys(begin).sort(), Object.keys(fixture("feedback-begin")).sort());
  assert.equal(begin.pngBytes, 0);
  assert.equal(begin.chunks, 0);
  assert.equal(begin.report.screenshot, null);
  assert.equal(r.sent.some(m => m.type === "feedback-chunk"), false, "no screenshot, no chunks");
});

// ---- history, live, resume ------------------------------------------------------------------

test("history goes first (this PC plus lines with no pc), then live lines, one batch in flight", async () => {
  const r = rig();
  r.log.setContext({ pc: null });
  r.log.info("t", "no pc 1");
  r.log.setContext({ pc: PC });
  r.log.info("t", "this pc 1");
  r.log.setContext({ pc: "other-pc" });
  r.log.info("t", "other pc");
  r.log.setContext({ pc: PC });
  r.log.info("t", "this pc 2");
  r.state0({});
  await r.advance(0);
  assert.equal(r.batches().length, 1);
  assert.deepEqual(lineMsgs(r.batches()[0]), ["no pc 1", "this pc 1", "this pc 2"]);
  assert.equal(r.batches()[0].live, false);

  r.log.info("t", "live 1");
  await r.advance(5000);
  assert.equal(r.batches().length, 1, "nothing more is sent before the ack");
  r.ack(r.batches()[0]);
  await r.advance(1000);
  assert.equal(r.batches().length, 2);
  assert.deepEqual(lineMsgs(r.batches()[1]), ["live 1"]);
  assert.equal(r.batches()[1].live, true);
  assert.equal(r.batches()[1].from, r.batches()[0].to);
  assert.equal(r.batches()[1].id, 2);
  r.ack(r.batches()[1]);
  r.log.info("t", "live 2");
  r.log.info("t", "live 3");
  await r.advance(1000);
  assert.deepEqual(lineMsgs(r.batches()[2]), ["live 2", "live 3"], "live lines coalesce into one batch");
});

test("a host cursor skips what it already holds, and nothing acked is sent twice after a dropped connection", async () => {
  const first = rig();
  first.lines(5);
  first.state0({});
  await first.advance(0);
  const [one] = first.batches();
  assert.equal(one.lines.length, 5);
  first.ack(one);
  first.lines(2);
  await first.advance(1000);
  const [, two] = first.batches();
  assert.deepEqual(lineMsgs(two), ["line 0", "line 1"]);
  // connection drops before the second ack; the host stored it anyway
  first.uploader.stop();

  const again = rig({ log: { raw: first.raw, log: first.log } });
  again.lines(1);
  again.state0({ browser: two.to }); // host's truth: the second batch was stored
  await again.advance(0);
  assert.deepEqual(again.batches().map(lineMsgs), [["line 0"]], "only the newer line goes out");
  // and a state from a host that stored only the first batch resends exactly the second
  const third = rig({ log: { raw: first.raw, log: first.log } });
  third.state0({ browser: one.to });
  await third.advance(0);
  assert.deepEqual(lineMsgs(third.batches()[0]), ["line 0", "line 1", "line 0"]);
  // host says the batch was already stored further ahead: resume from its cursor
  const r = rig();
  r.lines(3);
  r.state0({});
  await r.advance(0);
  r.ack(r.batches()[0], { cursor: formatCursor(Number(r.batches()[0].to) + 5) });
  r.lines(1);
  await r.advance(1000);
  assert.equal(r.batches().length, 1, "a lower new line is below the stored cursor");
});

// ---- ack statuses ---------------------------------------------------------------------------

test("rate-limited resends the same lines after retryAfterMs; rejected is not retried; budget stops", async () => {
  const r = rig();
  r.lines(2);
  r.state0({});
  await r.advance(0);
  const first = r.batches()[0];
  r.ack(first, { status: "rate-limited", retryAfterMs: 2000 });
  await r.advance(1900);
  assert.equal(r.batches().length, 1);
  await r.advance(200);
  assert.equal(r.batches().length, 2);
  assert.deepEqual(r.batches()[1].lines, first.lines);
  assert.equal(r.batches()[1].to, first.to);
  assert.notEqual(r.batches()[1].id, first.id);

  r.ack(r.batches()[1], { status: "rejected", reason: "batch too large" });
  await r.advance(5000);
  // the uploader's own note about the rejection is the only new line
  assert.equal(r.batches().length, 3);
  assert.deepEqual(lineMsgs(r.batches()[2]), ["host rejected a log batch"], "the rejected lines are not sent again");
  r.ack(r.batches()[2]);
  r.lines(1);
  await r.advance(1000);
  assert.deepEqual(lineMsgs(r.batches()[3]), ["line 0"], "later lines still go");

  r.ack(r.batches()[3], { status: "budget" });
  r.lines(3);
  await r.advance(60_000);
  assert.equal(r.batches().length, 4, "no more log traffic this connection");
});

test("the client also stops at the connection budget the host announced", async () => {
  const r = rig();
  r.lines(60);
  r.state0({}, { connectionBudgetBytes: 4000, maxBatchLines: 10 });
  await r.advance(0);
  for (let i = 0; i < 20 && !r.uploader.stats().budgetStopped; i++) {
    const last = r.batches().at(-1);
    if (last && r.uploader.stats().inflight) r.ack(last);
    await r.advance(600);
  }
  assert.equal(r.uploader.stats().budgetStopped, true);
  const total = r.batches().reduce((sum, m) => sum + Buffer.byteLength(JSON.stringify(m)), 0);
  assert.ok(total <= 4000, `sent ${total} bytes`);
  const count = r.batches().length;
  await r.advance(30_000);
  assert.equal(r.batches().length, count);
});

test("batches respect 100 lines and the byte limit; an oversize line is skipped, not stuck", async () => {
  const r = rig();
  r.lines(250);
  r.state0({});
  await r.advance(0);
  const sizeOf = batch => Buffer.byteLength(JSON.stringify(batch));
  assert.ok(r.batches()[0].lines.length <= 100 && r.batches()[0].lines.length > 50);
  assert.ok(sizeOf(r.batches()[0]) <= 16_384, "16 KiB limit");
  r.ack(r.batches()[0]);
  await r.advance(600);
  assert.ok(sizeOf(r.batches()[1]) <= 16_384);

  const few = rig();
  few.lines(30);
  few.state0({}, { maxBatchLines: 10 });
  await few.advance(0);
  assert.equal(few.batches()[0].lines.length, 10, "the line limit applies");

  const small = rig();
  for (let i = 0; i < 40; i++) small.log.info("t", "x".repeat(300) + i);
  small.state0({}, { maxBatchBytes: 4096 });
  await small.advance(0);
  for (let i = 0; i < 30; i++) {
    const last = small.batches().at(-1);
    if (small.uploader.stats().inflight) small.ack(last);
    await small.advance(600);
  }
  assert.ok(small.batches().length > 1);
  for (const batch of small.batches()) assert.ok(Buffer.byteLength(JSON.stringify(batch)) <= 4096);
  assert.equal(small.batches().flatMap(b => b.lines).length, 40, "all lines were uploaded exactly once");
});

// ---- priority -------------------------------------------------------------------------------

test("no send while the ctl buffer is above 16 KiB, right after input, or inside the interval", async () => {
  const r = rig();
  r.lines(1);
  r.state.buffered = 16_385;
  r.state0({});
  await r.advance(3000);
  assert.equal(r.sent.length, 0, "buffer above 16 KiB blocks sending");
  r.state.buffered = 16_384;
  await r.advance(300);
  assert.equal(r.batches().length, 1, "at 16 KiB it goes");

  const input = rig();
  input.lines(1);
  input.state0({});
  input.state.lastInput = input.t() + 100; // an input message at +100 ms
  await input.advance(300);
  assert.equal(input.sent.length, 0, "inside the 250 ms quiet window");
  await input.advance(100);
  assert.equal(input.sent.length, 1);

  const paced = rig();
  paced.lines(1);
  paced.state0({});
  await paced.advance(0);
  const t0 = paced.t();
  paced.ack(paced.batches()[0]);
  paced.lines(1);
  await paced.advance(400);
  assert.equal(paced.batches().length, 1, "minBatchIntervalMs (500) not over yet");
  await paced.advance(200);
  assert.equal(paced.batches().length, 2);
  assert.ok(paced.t() - t0 >= 500);
});

test("a live line that arrives while input keeps flowing waits; input never waits for logs", async () => {
  const r = rig();
  r.state0({});
  await r.advance(0);
  for (let i = 0; i < 10; i++) {
    r.state.lastInput = r.t();
    r.log.info("t", `busy ${i}`);
    await r.advance(100);
  }
  assert.equal(r.sent.length, 0);
  await r.advance(300);
  assert.equal(r.batches().length, 1);
  assert.equal(r.batches()[0].lines.length, 10);
});

// ---- failures never reach the session -------------------------------------------------------

test("send errors, refused sends and a dead session do nothing worse than stop uploading", async () => {
  const r = rig();
  r.lines(2);
  r.state0({});
  r.state.throwOnSend = true;
  await r.advance(5000);
  assert.equal(r.sent.length, 0);
  r.state.throwOnSend = false;
  await r.advance(5000);
  assert.equal(r.batches().length, 1, "the same lines go once sending works again");

  const refused = rig();
  refused.lines(1);
  refused.state.accept = false;
  refused.state0({});
  await refused.advance(5000);
  refused.state.accept = true;
  await refused.advance(5000);
  assert.equal(refused.batches().length, 1);

  const dead = rig();
  dead.lines(1);
  dead.state0({});
  dead.state.active = false;
  await dead.advance(5000);
  assert.equal(dead.sent.length, 0);
  assert.equal(dead.timers.length, 0, "a finished uploader leaves no timer");
  assert.equal(dead.uploader.handle({ type: "logs-ack", v: 1 }), false);
});

test("an unanswered batch is retried with the same lines", async () => {
  const r = rig();
  r.lines(2);
  r.state0({});
  await r.advance(0);
  await r.advance(25_000);
  assert.equal(r.batches().length, 2);
  assert.deepEqual(r.batches()[1].lines, r.batches()[0].lines);
});

// ---- feedback -------------------------------------------------------------------------------

const REPORT = { ...fixture("feedback-begin").report };

test("a saved report is sent, deleted only after ack ok, and the dialog says it was sent", async () => {
  const r = rig();
  const sentIds = [];
  r.log.onReportSent(id => sentIds.push(id));
  await r.log.saveReport({ ...REPORT, id: "rep-a" });
  r.uploader.start();
  await tick();
  await r.advance(0);
  const begin = r.sent.find(m => m.type === "feedback-begin");
  assert.equal(begin.id, "rep-a");
  assert.equal((await r.log.listReports()).length, 1, "still stored before the ack");
  r.uploader.handle({ type: "feedback-ack", v: 1, id: "other", status: "ok", reason: "" });
  await tick();
  assert.equal((await r.log.listReports()).length, 1, "an ack for another id does nothing");
  r.uploader.handle(fixtureAck("rep-a", "ok"));
  await tick();
  assert.deepEqual(await r.log.listReports(), []);
  assert.deepEqual(sentIds, ["rep-a"]);

  // reports saved while connected go out too, one at a time
  await r.log.saveReport({ ...REPORT, id: "rep-b" });
  await r.log.saveReport({ ...REPORT, id: "rep-c", created: "2026-11-02T13:51:00.000Z" });
  await tick();
  await r.advance(5000);
  const begins = r.sent.filter(m => m.type === "feedback-begin").map(m => m.id);
  assert.equal(begins.join(), "rep-a,rep-b", "the second waits for the first ack");
  r.uploader.handle(fixtureAck("rep-b", "ok"));
  await tick();
  await r.advance(5000);
  assert.equal(r.sent.filter(m => m.type === "feedback-begin").map(m => m.id).join(), "rep-a,rep-b,rep-c");
});

function fixtureAck(id, status, reason = "") { return { ...fixture("feedback-ack-ok"), id, status, reason }; }

test("retry, rejected and unanswered reports stay stored and wait for the next connection", async () => {
  const r = rig();
  for (const id of ["rep-r", "rep-x", "rep-t"]) await r.log.saveReport({ ...REPORT, id, created: `2026-11-02T13:5${id.length}:00.000Z` });
  r.uploader.start();
  await tick();
  await r.advance(0);
  r.uploader.handle(fixtureAck("rep-r", "retry", "busy"));
  await r.advance(1000);
  r.uploader.handle(fixtureAck("rep-x", "rejected", "too large"));
  await r.advance(1000);
  await r.advance(25_000); // rep-t never answered
  assert.deepEqual((await r.log.listReports()).map(x => x.id).sort(), ["rep-r", "rep-t", "rep-x"]);
  const attempts = r.sent.filter(m => m.type === "feedback-begin").map(m => m.id);
  assert.equal(new Set(attempts).size, attempts.length, "one attempt each per connection");
  r.uploader.stop();

  // next connection: all three are sent again
  const next = rig({ log: { raw: r.raw, log: r.log } });
  next.uploader.start();
  await tick();
  await next.advance(0);
  for (let i = 0; i < 3; i++) {
    const last = next.sent.filter(m => m.type === "feedback-begin").at(-1);
    next.uploader.handle(fixtureAck(last.id, "ok"));
    await tick();
    await next.advance(1000);
  }
  assert.deepEqual(await next.log.listReports(), []);
});

test("logs and reports share the priority rules: feedback also waits for a quiet, empty buffer", async () => {
  const r = rig();
  await r.log.saveReport({ ...REPORT, id: "rep-q" });
  r.state.buffered = 20_000;
  r.uploader.start();
  await tick();
  await r.advance(2000);
  assert.equal(r.sent.length, 0);
  r.state.buffered = 0;
  r.state.lastInput = r.t();
  await r.advance(200);
  assert.equal(r.sent.length, 0);
  await r.advance(100);
  assert.equal(r.sent.length, 1);
});

test("the dialog's saved line becomes 'Sent to your PC.' after the ack", async () => {
  const { log } = makeLog(() => 1_000_000);
  const listeners = [];
  const elements = [];
  const make = tag => {
    const handlers = {};
    const node = { tag, children: [], style: {}, hidden: false, textContent: "", value: "", attrs: {},
      setAttribute(k, v) { this.attrs[k] = v; if (k === "id") elements[v] = this; }, appendChild(c) { this.children.push(c); return c; },
      append(...c) { this.children.push(...c); }, addEventListener(t, f) { (handlers[t] ??= []).push(f); },
      querySelectorAll: () => [], focus() {}, set id(v) {}, className: "" };
    return node;
  };
  const doc = { createElement: make, body: make("body"), activeElement: null, addEventListener() {} };
  const feedback = createFeedback({ doc, log, nav: { language: "en" }, loc: { search: "?pc=pc-1" }, id: "rep-dialog" });
  feedback.open();
  const text = elements["bp-feedback-text"], status = elements["bp-feedback-status"];
  text.value = "It froze";
  await feedback.submit();
  assert.equal(status.textContent, SAVED_MEMORY_TEXT, "no IndexedDB in this test: kept in memory");
  log.reportSent("someone-else");
  assert.equal(status.textContent, SAVED_MEMORY_TEXT);
  log.reportSent("rep-dialog");
  assert.equal(status.textContent, SENT_TEXT);
  assert.equal(SENT_TEXT, "Sent to your PC.");
  void listeners;
});

test("the real logger exposes head(), deleteReport() and report events without throwing", async () => {
  const { raw } = makeLog(() => 1_000_000);
  assert.equal(raw.head(), 0);
  raw.info("t", "one");
  assert.ok(raw.head() > 0);
  const seen = [];
  const off = raw.onReportSaved(id => seen.push(id));
  raw.onReportSaved(() => { throw new Error("bad listener"); });
  await raw.saveReport({ id: "z1" });
  assert.equal(seen.join(), "z1");
  off();
  await raw.saveReport({ id: "z2" });
  assert.equal(seen.join(), "z1");
  assert.equal(await raw.deleteReport("z1"), true);
  assert.equal((await raw.listReports()).map(x => x.id).join(), "z2");
});

test("feedback-begin stays under 128 KiB: oldest logs are dropped first, then oldest errors", async () => {
  const entry = (i, size) => ({ ts: "2026-11-02T13:45:12.345Z", level: "info", src: "browser", msg: `n${i} ` + "x".repeat(size) });
  const big = { ...REPORT, id: "rep-big", logs: Array.from({ length: 200 }, (_, i) => entry(i, 900)),
    errors: Array.from({ length: 20 }, (_, i) => entry(1000 + i, 900)) };
  const r = rig();
  await r.log.saveReport(big);
  r.uploader.start();
  await tick();
  await r.advance(0);
  const begin = r.sent.find(m => m.type === "feedback-begin");
  assert.ok(Buffer.byteLength(JSON.stringify(begin)) <= MAX_FEEDBACK_BYTES);
  assert.ok(begin.report.logs.length < 200 && begin.report.logs.length > 0);
  assert.equal(begin.report.logs.at(-1).msg.split(" ")[0], "n199", "newest logs are kept");
  assert.equal(begin.report.errors.length, 20, "errors untouched while dropping logs is enough");

  const huge = { ...REPORT, id: "rep-huge", logs: Array.from({ length: 200 }, (_, i) => entry(i, 1500)),
    errors: Array.from({ length: 20 }, (_, i) => entry(1000 + i, 9000)) };
  const h = rig();
  await h.log.saveReport(huge);
  h.uploader.start();
  await tick();
  await h.advance(0);
  const second = h.sent.find(m => m.type === "feedback-begin");
  assert.ok(Buffer.byteLength(JSON.stringify(second)) <= MAX_FEEDBACK_BYTES);
  assert.equal(second.report.logs.length, 0);
  assert.ok(second.report.errors.length < 20);
  assert.equal(second.report.errors.at(-1).msg.split(" ")[0], "n1019");
  assert.equal((await h.log.listReports())[0].logs.length, 200, "the stored report is untouched");
});
