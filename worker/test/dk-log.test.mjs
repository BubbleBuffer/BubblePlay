import test from "node:test";
import { createSessionContext } from "./helpers/session-context.mjs";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";

const SOURCE = readFileSync(new URL("../public/dk-log.js", import.meta.url), "utf8");

/** Loads dk-log.js into a fresh context, like a classic script. */
function loadScript(globals = {}) {
  const listeners = {};
  const context = createSessionContext({
    console: { warn() {}, error() {}, log() {} },
    addEventListener: (type, fn) => (listeners[type] ??= []).push(fn),
    setTimeout: () => 1,
    Date, JSON, Error, Promise, Set, Math, Number, String, Array, Object, RegExp,
    ...globals,
  });
  context.globalThis = context;
  vm.runInContext(SOURCE, context);
  return { context, listeners };
}

/** A small IndexedDB stand-in: enough for put, delete, getAll and a cursor scan. */
function fakeIndexedDB({ failOpen = false } = {}) {
  const data = { lines: new Map(), feedback: new Map() };
  const created = new Set();
  const state = { data, opens: 0, indexes: [] };
  const db = {
    objectStoreNames: { contains: name => created.has(name) },
    createObjectStore(name, options) {
      created.add(name);
      state[`${name}Options`] = options;
      return { createIndex: (index, path) => state.indexes.push([name, index, path]) };
    },
    close() {},
    transaction(name) {
      const map = data[name];
      const tx = {};
      const done = () => queueMicrotask(() => tx.oncomplete?.());
      tx.objectStore = () => ({
        put: (value, key) => {
          const k = key ?? value.id;
          map.set(k, structuredClone(value));
          const request = {};
          done();
          return request;
        },
        delete: key => { map.delete(key); done(); return {}; },
        getAll: () => {
          const request = { result: [...map.values()] };
          queueMicrotask(() => request.onsuccess?.());
          return request;
        },
        openCursor: () => {
          const keys = [...map.keys()].sort((a, b) => a - b);
          const request = { result: null };
          let at = 0;
          const step = () => queueMicrotask(() => {
            request.result = at < keys.length
              ? { key: keys[at], value: structuredClone(map.get(keys[at])), continue() { at++; step(); } }
              : null;
            request.onsuccess?.();
          });
          step();
          return request;
        },
      });
      return tx;
    },
  };
  return {
    state,
    open() {
      state.opens++;
      const request = { result: db };
      queueMicrotask(() => {
        if (failOpen) return request.onerror?.();
        if (!created.size) request.onupgradeneeded?.();
        request.onsuccess?.();
      });
      return request;
    },
  };
}

const tick = () => new Promise(resolve => setTimeout(resolve, 5));

function make(extra = {}) {
  let t = 1_800_000_000_000;
  const timers = [];
  const { context, listeners } = loadScript({ indexedDB: undefined });
  const env = { now: () => t++, setTimeout: fn => { timers.push(fn); return timers.length; },
    indexedDB: undefined, console: { warn() {}, error() {} }, document: { visibilityState: "visible" }, ...extra };
  const raw = context.BPLogCreate(env);
  // Results cross a vm realm; round-trip them so deepStrictEqual compares plain objects.
  const plain = value => JSON.parse(JSON.stringify(value ?? null));
  const log = { ...raw };
  for (const name of ["recent", "errors", "drain", "stats", "context"]) log[name] = (...args) => plain(raw[name](...args));
  for (const name of ["saveReport", "listReports"]) log[name] = async (...args) => plain(await raw[name](...args));
  return { log, env, timers, context, listeners, runTimers: () => { while (timers.length) timers.shift()(); } };
}

// ---- schema and API ------------------------------------------------------------------------

test("a line follows the JSONL schema with src browser and the set context", () => {
  const { log } = make();
  log.setContext({ pc: "B7QK-2M9X-H4PD-R8TN", session: "s-1", device: "dev-1" });
  log.info("dk-app", "connected", { transport: "wss" });
  const [line] = log.recent(5);
  assert.deepEqual(Object.keys(line), ["ts", "level", "src", "device", "pc", "session", "target", "msg", "fields"]);
  assert.match(line.ts, /^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{3}Z$/);
  assert.equal(line.src, "browser");
  assert.equal(line.level, "info");
  assert.equal(line.pc, "B7QK-2M9X-H4PD-R8TN");
  assert.equal(line.session, "s-1");
  assert.equal(line.device, "dev-1");
  assert.deepEqual(line.fields, { transport: "wss" });
  assert.doesNotThrow(() => JSON.parse(JSON.stringify(line)));
});

test("recent and errors return the newest lines; debug is not kept", () => {
  const { log } = make();
  log.log("debug", "t", "quiet");
  for (let i = 0; i < 5; i++) log.info("t", `info ${i}`);
  log.error("t", "bad one");
  log.warn("t", "careful");
  log.error("t", "bad two");
  assert.equal(log.recent(100).length, 8);
  assert.deepEqual(log.recent(2).map(l => l.msg), ["careful", "bad two"]);
  assert.deepEqual(log.errors(5).map(l => l.msg), ["bad one", "bad two"]);
  assert.deepEqual(log.errors(1).map(l => l.msg), ["bad two"]);
});

test("a long message is cut and an oversized line is reduced to fit 8 KiB", () => {
  const { log } = make();
  log.info("t", "x".repeat(20_000), { big: "y".repeat(50_000), stack: "s".repeat(20_000) });
  const [line] = log.recent(1);
  assert.ok(JSON.stringify(line).length <= 8192);
  assert.match(line.msg, /\.\.\.\[truncated\]$/);
});

test("the API never throws, even on cyclic fields and odd input", () => {
  const { log } = make();
  const cyc = {}; cyc.self = cyc;
  assert.doesNotThrow(() => log.info("t", "m", cyc));
  assert.doesNotThrow(() => log.info(undefined, undefined, 42));
  assert.doesNotThrow(() => log.error(null, { toString() { throw new Error("no"); } }));
  assert.doesNotThrow(() => log.setContext(null));
  assert.doesNotThrow(() => log.drain(undefined, "x"));
  assert.ok(log.recent(10).length >= 2);
});

// ---- drain cursor --------------------------------------------------------------------------

test("drain returns this PC's lines and the not-PC lines with a rising cursor", () => {
  const { log } = make();
  log.info("t", "no pc");
  log.setContext({ pc: "PC-A" });
  log.info("t", "a1");
  log.setContext({ pc: "PC-B" });
  log.info("t", "b1");
  log.setContext({ pc: "PC-A" });
  log.info("t", "a2");
  const all = log.drain("PC-A", 0);
  assert.deepEqual(all.map(item => item.line.msg), ["no pc", "a1", "a2"]);
  assert.ok(all.every((item, i) => i === 0 || item.cursor > all[i - 1].cursor));
  const rest = log.drain("PC-A", all[1].cursor);
  assert.deepEqual(rest.map(item => item.line.msg), ["a2"]);
  assert.deepEqual(log.drain("PC-A", all[2].cursor), []);
  assert.equal(log.drain("PC-A", 0, 2).length, 2);
});

// ---- ring cap ------------------------------------------------------------------------------

test("the ring is capped and evicts the oldest lines first", () => {
  const { log } = make({ capBytes: 4000 });
  for (let i = 0; i < 100; i++) log.info("t", `line ${i} ${"p".repeat(50)}`);
  const stats = log.stats();
  assert.ok(stats.bytes <= 4000, `bytes ${stats.bytes}`);
  const lines = log.recent(1000);
  assert.ok(lines.length < 100 && lines.length > 5);
  assert.match(lines.at(-1).msg, /^line 99 /);
  assert.ok(!lines.some(l => l.msg.startsWith("line 0 ")));
  const cursors = log.drain(null, 0).map(item => item.cursor);
  assert.deepEqual(cursors, [...cursors].sort((a, b) => a - b));
});

test("the default cap is 2 MB", () => {
  assert.equal(make().log.stats().capBytes, 2 * 1024 * 1024);
});

test("IndexedDB holds the ring: lines are written, evicted rows deleted, and a reload sees them", async () => {
  const idb = fakeIndexedDB();
  const first = make({ indexedDB: idb, capBytes: 3000 });
  await tick();
  assert.equal(first.log.state(), "ready");
  assert.deepEqual(idb.state.indexes, [["lines", "pc", "pc"]]);
  assert.equal(idb.state.linesOptions.autoIncrement, true);
  for (let i = 0; i < 60; i++) first.log.info("t", `row ${i} ${"q".repeat(40)}`);
  first.runTimers();
  await tick();
  const stored = [...idb.state.data.lines.values()];
  assert.ok(stored.length > 3 && stored.length === first.log.recent(1000).length, "store mirrors the ring");
  assert.match(stored.at(-1).msg, /^row 59 /);
  assert.ok(!stored.some(r => r.msg.startsWith("row 0 ")), "evicted rows are deleted");

  let later = 1_900_000_000_000;
  const second = make({ indexedDB: idb, capBytes: 3000, now: () => later++ });
  second.log.info("t", "after reload");
  await tick();
  const lines = second.log.recent(1000);
  assert.ok(lines.some(l => l.msg.startsWith("row 59 ")));
  assert.equal(lines.at(-1).msg, "after reload");
  const cursors = second.log.drain(null, 0).map(item => item.cursor);
  assert.deepEqual(cursors, [...cursors].sort((a, b) => a - b));
  assert.ok(second.log.stats().bytes <= 3000);
});

test("without IndexedDB the in-memory ring still works", () => {
  const { log } = make({ indexedDB: undefined });
  assert.equal(log.state(), "memory");
  log.info("t", "kept in memory");
  assert.equal(log.recent(1)[0].msg, "kept in memory");
  assert.doesNotThrow(() => log.flush());
});

test("a failing IndexedDB open falls back to memory", async () => {
  const { log } = make({ indexedDB: fakeIndexedDB({ failOpen: true }) });
  await tick();
  assert.equal(log.state(), "memory");
  log.info("t", "still logged");
  assert.equal(log.recent(1)[0].msg, "still logged");
});

test("an indexedDB that throws on open is survived", () => {
  const { log } = make({ indexedDB: { open() { throw new Error("denied"); } } });
  assert.equal(log.state(), "memory");
  log.warn("t", "ok");
  assert.equal(log.recent(1)[0].msg, "ok");
});

// ---- capture -------------------------------------------------------------------------------

test("window error and unhandledrejection are logged with target uncaught", () => {
  const listeners = {};
  const target = { addEventListener: (type, fn) => (listeners[type] ??= []).push(fn) };
  const { log } = make({ target, console: { warn() {}, error() {} } });
  log.install();
  const error = new TypeError("boom");
  listeners.error[0]({ message: "Uncaught TypeError: boom", filename: "https://x.test/receiver.js", lineno: 12, colno: 3, error });
  listeners.unhandledrejection[0]({ reason: new Error("rejected") });
  listeners.unhandledrejection[0]({ reason: "plain string" });
  const lines = log.errors(10);
  assert.equal(lines.length, 3);
  assert.ok(lines.every(l => l.target === "uncaught" && l.level === "error"));
  assert.equal(lines[0].fields.error, "boom");
  assert.match(lines[0].fields.stack, /TypeError: boom/);
  assert.equal(lines[0].fields.line, 12);
  assert.equal(lines[1].fields.error, "rejected");
  assert.equal(lines[2].fields.error, "plain string");
});

test("console.warn and console.error are mirrored and still reach the console", () => {
  const seen = [];
  const sink = { warn: (...a) => seen.push(["warn", ...a]), error: (...a) => seen.push(["error", ...a]) };
  const { log } = make({ target: { addEventListener() {} }, console: sink });
  log.install();
  sink.warn("careful", { a: 1 });
  sink.error(new Error("failed"));
  assert.equal(seen.length, 2);
  const lines = log.recent(5);
  assert.deepEqual(lines.map(l => [l.level, l.target]), [["warn", "console"], ["error", "console"]]);
  assert.equal(lines[0].msg, 'careful {"a":1}');
  assert.match(lines[1].msg, /Error: failed/);
});

test("the installed global logger exists after the script runs and catches errors", () => {
  const { context, listeners } = loadScript({ indexedDB: undefined });
  assert.equal(typeof context.BPLog.info, "function");
  listeners.error[0]({ message: "script failed", error: new Error("x") });
  assert.equal(context.BPLog.errors(1)[0].target, "uncaught");
});

// ---- redaction corpus (spec section 2, rules 1-6) ------------------------------------------

const R = "[redacted]";

test("a WebSocket closeCode (1006, 1001) is logged, other code keys stay redacted", () => {
  const { log } = make();
  log.info("t", "signaling-close", { closeCode: 1006, code: "7K3QD-M9X2P", pairingCode: 1001, authCloseCode: 1001 });
  const f = log.recent(1)[0].fields;
  assert.equal(f.closeCode, 1006);
  assert.equal(f.code, R);
  assert.equal(f.pairingCode, R);
  assert.equal(f.authCloseCode, R);
});

test("the reconnect fields (connectAttempt, waitedMs, event) pass the redactor", () => {
  const { log } = make();
  log.info("t", "m", { connectAttempt: 2, waitedMs: 5000, event: "visibilitychange" });
  assert.deepEqual({ ...log.recent(1)[0].fields }, { connectAttempt: 2, waitedMs: 5000, event: "visibilitychange" });
});

test("rule 1: fields with secret-looking keys lose the whole value", () => {
  const { log } = make();
  log.info("t", "m", {
    password: "hunter2", passphrase: "correct horse", Passwd: "x", clientSecret: "s", access_token: "tok",
    deviceKey: "k", proof: "p", ticket: "t", pairingCode: "7K3QD-M9X2P", psk: "psk", signature: "sig",
    "auth-header": "h", Cookie: "c", credentials: { user: "u" }, nested: { apiKey: "abc" }, list: [{ token: "t" }],
  });
  const f = log.recent(1)[0].fields;
  for (const name of ["password", "passphrase", "Passwd", "clientSecret", "access_token", "deviceKey", "proof", "ticket",
    "pairingCode", "psk", "signature", "auth-header", "Cookie", "credentials"]) assert.equal(f[name], R, name);
  assert.equal(f.nested.apiKey, R);
  assert.equal(f.list[0].token, R);
});

test("rule 1: allow-listed names and plain values pass", () => {
  const { log } = make();
  log.info("t", "m", { keyboard: "us", keyframe: "requested", keycode: "KeyA", keyId: "k-1", device_key_id: "d-1",
    codec: "h264", decoder: "hw", code: 1008, reason: "policy", authenticated: true, count: 3, name: "Edge on Windows" });
  assert.deepEqual(log.recent(1)[0].fields, { keyboard: "us", keyframe: "requested", keycode: "KeyA", keyId: "k-1",
    device_key_id: "d-1", codec: "h264", decoder: "hw", code: 1008, reason: "policy", authenticated: true, count: 3,
    name: "Edge on Windows" });
});

test("rule 1: a code field that is text is replaced, a numeric status is kept", () => {
  const { log } = make();
  log.info("t", "m", { code: "7K3QD-M9X2P" });
  log.info("t", "m", { code: 1006 });
  const [a, b] = log.recent(2);
  assert.equal(a.fields.code, R);
  assert.equal(b.fields.code, 1006);
});

test("rule 2: URL query and fragment params are replaced", () => {
  const { log } = make();
  log.info("t", "open https://relay.test/ws?pc=ABC&ticket=abc123&x=1 and /pair#pc=B7QK&code=7K3QD-M9X2P&other=y");
  log.info("t", "m", { href: "https://h.test/a?token=zzz&key=kkk&Sig=s&password=p&pairing=q&proof=r&secret=s&passphrase=pp" });
  const [msg, fields] = log.recent(2);
  assert.equal(msg.msg, `open https://relay.test/ws?pc=ABC&ticket=${R}&x=1 and /pair#pc=B7QK&code=${R}&other=y`);
  assert.equal(fields.fields.href,
    `https://h.test/a?token=${R}&key=${R}&Sig=${R}&password=${R}&pairing=${R}&proof=${R}&secret=${R}&passphrase=${R}`);
});

test("rule 3: Bearer tokens and Authorization header lines", () => {
  const { log } = make();
  log.info("t", "header Bearer abc.def-ghi and more");
  log.info("t", "request\nAuthorization: Basic dXNlcjpwYXNz\nAccept: */*");
  const [a, b] = log.recent(2);
  assert.equal(a.msg, `header Bearer ${R} and more`);
  assert.equal(b.msg, `request\nAuthorization: ${R}\nAccept: */*`);
});

test("rule 4: key=value, key: value and JSON pairs in free text", () => {
  const { log } = make();
  log.info("t", "login token=abc123 user=bob");
  log.info("t", "passphrase: correct horse battery staple");
  log.info("t", 'body {"password":"hunter 2","name":"x"}');
  log.info("t", "reason: bad ticket=zz9 next=1");
  log.info("t", "closed code=1008 reason=policy");
  const msgs = log.recent(5).map(l => l.msg);
  assert.equal(msgs[0], `login token=${R} user=bob`);
  assert.equal(msgs[1], `passphrase: ${R}`);
  assert.equal(msgs[2], `body {"password":"${R}","name":"x"}`);
  assert.equal(msgs[3], `reason: bad ticket=${R} next=1`);
  assert.equal(msgs[4], "closed code=1008 reason=policy");
});

test("rule 5: a registered literal never appears, longest first", () => {
  const { log } = make();
  log.registerSecret("hunter2");
  log.registerSecret("hunter2-and-more");
  log.info("t", "typed hunter2-and-more then hunter2", { note: "has hunter2 inside", list: ["hunter2"] });
  const line = log.recent(1)[0];
  assert.equal(line.msg, `typed ${R} then ${R}`);
  assert.equal(line.fields.note, `has ${R} inside`);
  assert.deepEqual(line.fields.list, [R]);
  assert.ok(!JSON.stringify(log.recent(10)).includes("hunter2"));
});

test("rule 6: long runs of key-like characters become [redacted-blob]; ids pass", () => {
  const { log } = make();
  const blob = "A1b2C3d4".repeat(6);
  log.info("t", `key material ${blob} end`, { value: blob, uuid: "11111111-2222-4333-8444-555555555555", pc: blob, short: "A1b2C3d4" });
  const line = log.recent(1)[0];
  assert.equal(line.msg, "key material [redacted-blob] end");
  assert.equal(line.fields.value, "[redacted-blob]");
  assert.equal(line.fields.uuid, "11111111-2222-4333-8444-555555555555");
  assert.equal(line.fields.pc, blob, "ids in the dedicated names are left alone");
  assert.equal(line.fields.short, "A1b2C3d4");
});

test("redaction applies to the target, error text and stacks", () => {
  const { log } = make();
  log.error("t", "failed", { error: "bad token=abcd", stack: "Error\n at https://x.test/a.js?ticket=zz:1:2" });
  const f = log.recent(1)[0].fields;
  assert.equal(f.error, `bad token=${R}`);
  assert.match(f.stack, new RegExp(`ticket=\\${R.slice(0, 1)}redacted\\]`));
});

test("secrets never reach IndexedDB or the memory ring", async () => {
  const idb = fakeIndexedDB();
  const { log, runTimers } = make({ indexedDB: idb });
  await tick();
  log.registerSecret("my-passphrase-123");
  log.registerSecret("7K3QD-M9X2P");
  log.info("pairing", "using my-passphrase-123 for 7K3QD-M9X2P", { passphrase: "my-passphrase-123", code: "7K3QD-M9X2P",
    url: "https://r.test/pair#pc=B7QK-2M9X-H4PD-R8TN&code=7K3QD-M9X2P", header: "Bearer eyJhbGciOiJFZERTQSJ9.payload.sig" });
  log.error("auth", "ticket=abcdef0123456789 refused", { ticket: "abcdef0123456789", key: "k".repeat(64) });
  runTimers();
  await tick();
  const everything = JSON.stringify([...idb.state.data.lines.values()]) + JSON.stringify(log.recent(100)) +
    JSON.stringify(log.drain(null, 0));
  for (const secret of ["my-passphrase-123", "7K3QD-M9X2P", "eyJhbGciOiJFZERTQSJ9", "abcdef0123456789", "kkkkkkkkkk"])
    assert.ok(!everything.includes(secret), `${secret} must not be stored`);
});

test("redaction failure replaces the line with a plain notice", () => {
  const { log } = make();
  log.registerSecret({ toString() { throw new Error("x"); } });
  log.info("t", "after bad registration", { a: 1 });
  assert.equal(log.recent(1)[0].msg, "after bad registration");
  const hostile = { get boom() { throw new Error("getter"); } };
  assert.doesNotThrow(() => log.info("t", "m", hostile));
  assert.ok(log.recent(1)[0]);
  assert.equal(log.redact({ toString() { throw new Error("no"); } }), "[redaction failed]");
});

// ---- feedback store ------------------------------------------------------------------------

test("saveReport writes to the feedback store, or keeps the report in memory without one", async () => {
  const idb = fakeIndexedDB();
  const stored = make({ indexedDB: idb });
  await tick();
  const result = await stored.log.saveReport({ id: "r-1", text: "hello" });
  assert.deepEqual(result, { ok: true, persisted: true });
  assert.equal(idb.state.data.feedback.get("r-1").text, "hello");
  assert.equal((await stored.log.listReports()).length, 1);
  assert.equal(idb.state.feedbackOptions.keyPath, "id");

  const memory = make({ indexedDB: undefined });
  assert.deepEqual(await memory.log.saveReport({ id: "r-2" }), { ok: true, persisted: false });
  assert.equal((await memory.log.listReports())[0].id, "r-2");
});
