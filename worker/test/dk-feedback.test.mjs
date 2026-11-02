import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";
import {
  buildReport, createFeedback, currentRoute, installFeedback, FEEDBACK_KINDS, SAVED_TEXT, SAVED_MEMORY_TEXT,
} from "../public/dk-feedback.js";

// ---- a real logger (dk-log.js) in its own context, results flattened to plain objects --------
const SOURCE = readFileSync(new URL("../public/dk-log.js", import.meta.url), "utf8");
const plain = value => JSON.parse(JSON.stringify(value ?? null));

function makeLog({ persisted = false } = {}) {
  const context = vm.createContext({ console: { warn() {}, error() {} }, addEventListener() {}, setTimeout: () => 1,
    Date, JSON, Error, Promise, Set, Math, Number, String, Array, Object, RegExp, indexedDB: undefined });
  context.globalThis = context;
  vm.runInContext(SOURCE, context);
  const raw = context.BPLogCreate({ indexedDB: undefined, setTimeout: () => 1, console: { warn() {}, error() {} } });
  const saved = [];
  return {
    saved,
    log: {
      info: (...a) => raw.info(...a), warn: (...a) => raw.warn(...a), error: (...a) => raw.error(...a),
      setContext: c => raw.setContext(c), registerSecret: s => raw.registerSecret(s),
      context: () => plain(raw.context()), redact: t => raw.redact(t),
      recent: n => plain(raw.recent(n)), errors: n => plain(raw.errors(n)),
      saveReport: async report => { saved.push(plain(report)); return { ok: true, persisted }; },
    },
  };
}

const NAV = { userAgent: "Mozilla/5.0 (Windows NT 10.0) Chrome/140.0", userAgentData: { brands: [{ brand: "Google Chrome" }], platform: "Windows" }, language: "en-GB" };
const LOC = { search: "?pc=B7QK-2M9X-H4PD-R8TN" };
const DOC = { getElementById: () => null };
const FIXED = () => new Date("2026-11-02T13:45:12.345Z");

// ---- report building -------------------------------------------------------------------------

test("buildReport follows the spec shape with logs, errors and context", () => {
  const { log } = makeLog();
  log.setContext({ device: "dev-1", pc: "B7QK-2M9X-H4PD-R8TN", session: "s-9", build: "native-2026" });
  for (let i = 0; i < 230; i++) log.info("t", `line ${i}`);
  log.error("t", "bad thing", { error: "boom" });
  const report = plain(buildReport({ text: "The picture froze", kind: "bug", log, nav: NAV, loc: LOC, doc: DOC, now: FIXED, id: "11111111-2222-4333-8444-555555555555" }));
  assert.deepEqual(Object.keys(report), ["version", "id", "created", "text", "kind", "app", "platform", "device", "pc", "session",
    "route", "locale", "logs", "errors"]);
  assert.equal(report.version, 1);
  assert.equal(report.id, "11111111-2222-4333-8444-555555555555");
  assert.equal(report.created, "2026-11-02T13:45:12.345Z");
  assert.equal(report.text, "The picture froze");
  assert.equal(report.kind, "bug");
  assert.deepEqual(report.app, { version: "web", build: "native-2026", commit: null });
  assert.equal(report.platform, "Windows");
  assert.deepEqual(report.device, { id: "dev-1", name: "Chrome on Windows", model: null });
  assert.equal(report.pc, "B7QK-2M9X-H4PD-R8TN");
  assert.equal(report.session, "s-9");
  assert.equal(report.route, "session");
  assert.equal(report.locale, "en-GB");
  assert.equal(report.logs.length, 200);
  assert.equal(report.logs.at(-1).msg, "bad thing");
  assert.equal(report.errors.length, 1);
  assert.ok(!("screenshot" in report), "the browser never sends a screenshot");
});

test("buildReport redacts the text and the metadata strings", () => {
  const { log } = makeLog();
  log.registerSecret("my-secret-phrase");
  const report = plain(buildReport({
    text: "I typed my-secret-phrase and then token=abc123 failed at https://r.test/pair#code=7K3QD-M9X2P\nAuthorization: Bearer xyz",
    kind: "complaint", log, nav: { ...NAV, language: "en token=zzz" }, loc: LOC, doc: DOC, now: FIXED }));
  assert.ok(!report.text.includes("my-secret-phrase"));
  assert.ok(!report.text.includes("abc123"));
  assert.ok(!report.text.includes("7K3QD-M9X2P"));
  assert.ok(!report.text.includes("xyz"));
  assert.match(report.text, /\[redacted\]/);
  assert.ok(!report.locale.includes("zzz"));
});

test("the log lines in a report are the already redacted ones", () => {
  const { log } = makeLog();
  log.error("auth", "failed with ticket=SECRETTICKET1", { passphrase: "hunter2", url: "https://x.test/?key=KEYVALUE9" });
  const report = plain(buildReport({ text: "see logs", log, nav: NAV, loc: LOC, doc: DOC, now: FIXED }));
  const all = JSON.stringify(report);
  for (const secret of ["SECRETTICKET1", "hunter2", "KEYVALUE9"]) assert.ok(!all.includes(secret), secret);
});

test("an unknown kind falls back to other, and a long text is cut", () => {
  const { log } = makeLog();
  const report = plain(buildReport({ text: "a ".repeat(5000), kind: "<script>", log, nav: NAV, loc: LOC, doc: DOC, now: FIXED }));
  assert.equal(report.kind, "other");
  assert.ok(report.text.length <= 4000);
  assert.deepEqual(FEEDBACK_KINDS.map(k => k.label), ["Bug", "Complaint", "Idea", "Other"]);
});

test("buildReport refuses to build without the logger so nothing unredacted is stored", () => {
  assert.throws(() => buildReport({ text: "x", log: null, nav: NAV, loc: LOC, doc: DOC }));
});

test("currentRoute tells the gate, the list and a session apart", () => {
  const visibleGate = { getElementById: id => (id === "dk-gate" ? { hidden: false } : null) };
  const hiddenGate = { getElementById: id => (id === "dk-gate" ? { hidden: true } : null) };
  assert.equal(currentRoute(visibleGate, LOC), "gate");
  assert.equal(currentRoute(hiddenGate, LOC), "session");
  assert.equal(currentRoute(hiddenGate, { search: "" }), "list");
});

// ---- the dialog, on a minimal fake document -------------------------------------------------

class Node {
  constructor(doc, tag) {
    this.ownerDocument = doc; this.tagName = tag.toUpperCase(); this.children = []; this.parentNode = null;
    this.attributes = {}; this.listeners = {}; this.className = ""; this.hidden = false; this.disabled = false;
    this.readOnly = false; this.value = ""; this.text = ""; this.id = "";
  }
  get textContent() { return this.text + this.children.map(c => c.textContent).join(""); }
  set textContent(v) { this.text = String(v); this.children = []; }
  setAttribute(name, value) { this.attributes[name] = String(value); if (name === "id") this.id = String(value); }
  appendChild(node) { node.parentNode = this; this.children.push(node); return node; }
  append(...nodes) { nodes.forEach(n => this.appendChild(n)); }
  addEventListener(type, fn) { (this.listeners[type] ??= []).push(fn); }
  focus() { this.ownerDocument.activeElement = this; }
  querySelectorAll(selector) {
    const tags = selector.split(",").map(s => s.trim().toUpperCase());
    const out = [];
    const walk = node => { for (const c of node.children) { if (tags.includes(c.tagName)) out.push(c); walk(c); } };
    walk(this);
    return out;
  }
}

function makeDoc() {
  const doc = { listeners: {}, activeElement: null };
  doc.body = new Node(doc, "body");
  doc.createElement = tag => new Node(doc, tag);
  doc.addEventListener = (type, fn, capture) => (doc.listeners[type] ??= []).push({ fn, capture });
  doc.getElementById = id => {
    let found = null;
    const walk = node => { if (node.id === id) found = node; node.children.forEach(walk); };
    walk(doc.body);
    return found;
  };
  return doc;
}

/** Dispatches an event at a node and bubbles it until a handler stops it. */
function dispatch(node, type, props = {}) {
  const event = { type, target: node, defaultPrevented: false, stopped: false, ...props,
    preventDefault() { this.defaultPrevented = true; }, stopPropagation() { this.stopped = true; } };
  for (let at = node; at && !event.stopped; at = at.parentNode) for (const fn of at.listeners[type] ?? []) fn(event);
  return event;
}

function dialogSetup(options = { persisted: true }) {
  const doc = makeDoc();
  const { log, saved } = makeLog(options);
  const feedback = createFeedback({ doc, log, nav: NAV, loc: LOC, now: FIXED, id: "rid-1" });
  return { doc, log, saved, feedback };
}

test("the dialog has the spec's labels, kinds and buttons", () => {
  const { doc, feedback } = dialogSetup();
  const opener = doc.createElement("button"); doc.body.appendChild(opener); opener.focus();
  feedback.open();
  const root = doc.getElementById("bp-feedback");
  assert.equal(root.hidden, false);
  const dialog = root.children[0];
  assert.equal(dialog.attributes.role, "dialog");
  assert.equal(dialog.attributes["aria-modal"], "true");
  assert.equal(doc.getElementById(dialog.attributes["aria-labelledby"]).textContent, "Report a problem");
  const text = JSON.stringify(dialog, (k, v) => (k === "ownerDocument" || k === "parentNode" || k === "listeners" ? undefined : v));
  assert.match(text, /What happened\?/);
  for (const word of ["Bug", "Complaint", "Idea", "Other", "Send", "Cancel"]) assert.match(text, new RegExp(word));
  assert.equal(doc.activeElement.id, "bp-feedback-text", "focus moves to the text box");
  const labelFor = [];
  const walk = n => { if (n.tagName === "LABEL") labelFor.push(n.attributes.for); n.children.forEach(walk); };
  walk(dialog);
  assert.deepEqual(labelFor, ["bp-feedback-text", "bp-feedback-kind"]);
  assert.ok(doc.getElementById("bp-feedback-text") && doc.getElementById("bp-feedback-kind"));
  assert.equal(doc.getElementById("bp-feedback-status").attributes["aria-live"], "polite");
});

test("Escape and Cancel close the dialog and return focus", () => {
  const { doc, feedback } = dialogSetup();
  const opener = doc.createElement("button"); doc.body.appendChild(opener); opener.focus();
  feedback.open();
  const field = doc.getElementById("bp-feedback-text");
  assert.equal(dispatch(field, "keydown", { key: "Escape" }).defaultPrevented, true);
  assert.equal(feedback.isOpen(), false);
  assert.equal(doc.activeElement, opener);
  feedback.open();
  dispatch(doc.getElementById("bp-feedback-cancel"), "click");
  assert.equal(feedback.isOpen(), false);
  feedback.open();
  dispatch(doc.getElementById("bp-feedback"), "click", {});
  assert.equal(feedback.isOpen(), false, "a click on the scrim closes it");
});

test("Tab is trapped inside the dialog", () => {
  const { doc, feedback } = dialogSetup();
  feedback.open();
  const ids = ["bp-feedback-text", "bp-feedback-kind", "bp-feedback-send", "bp-feedback-cancel"];
  const last = doc.getElementById("bp-feedback-cancel");
  last.focus();
  assert.equal(dispatch(last, "keydown", { key: "Tab", shiftKey: false }).defaultPrevented, true);
  assert.equal(doc.activeElement.id, ids[0]);
  const first = doc.getElementById(ids[0]);
  assert.equal(dispatch(first, "keydown", { key: "Tab", shiftKey: true }).defaultPrevented, true);
  assert.equal(doc.activeElement.id, ids.at(-1));
  const mid = doc.getElementById("bp-feedback-kind"); mid.focus();
  assert.equal(dispatch(mid, "keydown", { key: "Tab", shiftKey: false }).defaultPrevented, false, "middle tabs move normally");
});

test("typing in the dialog does not reach the page's input handlers", () => {
  const { doc, feedback } = dialogSetup();
  feedback.open();
  const reached = [];
  doc.body.addEventListener("keydown", () => reached.push("keydown"));
  doc.body.addEventListener("paste", () => reached.push("paste"));
  dispatch(doc.getElementById("bp-feedback-text"), "keydown", { key: "a" });
  dispatch(doc.getElementById("bp-feedback-text"), "paste", {});
  assert.deepEqual(reached, []);
});

test("Send needs text, saves a redacted report and shows the saved line", async () => {
  const { doc, feedback, saved, log } = dialogSetup();
  log.setContext({ pc: "B7QK-2M9X-H4PD-R8TN" });
  feedback.open();
  const field = doc.getElementById("bp-feedback-text");
  await feedback.submit();
  assert.equal(saved.length, 0);
  assert.equal(doc.getElementById("bp-feedback-status").textContent, "Describe what happened first.");
  field.value = "It dropped. my passphrase=open-sesame was visible";
  doc.getElementById("bp-feedback-kind").value = "complaint";
  await feedback.submit();
  assert.equal(saved.length, 1);
  assert.equal(saved[0].kind, "complaint");
  assert.equal(saved[0].id, "rid-1");
  assert.ok(!JSON.stringify(saved[0]).includes("open-sesame"));
  assert.equal(doc.getElementById("bp-feedback-status").textContent, SAVED_TEXT);
  assert.equal(SAVED_TEXT, "Saved. It is sent to your PC the next time you connect.");
  assert.equal(doc.getElementById("bp-feedback-send").hidden, true);
  assert.equal(doc.getElementById("bp-feedback-cancel").textContent, "Close");
  await feedback.submit();
  assert.equal(saved.length, 1, "a saved report is not saved twice");
  assert.ok(log.recent(5).some(line => line.target === "feedback" && line.msg === "report saved"));
});

test("a report kept only in memory says so", async () => {
  const { doc, feedback } = dialogSetup({ persisted: false });
  feedback.open();
  doc.getElementById("bp-feedback-text").value = "hello";
  await feedback.submit();
  assert.equal(doc.getElementById("bp-feedback-status").textContent, SAVED_MEMORY_TEXT);
});

test("a failing save keeps the dialog open and allows another try", async () => {
  const doc = makeDoc();
  const { log } = makeLog();
  const feedback = createFeedback({ doc, log: { ...log, saveReport: async () => { throw new Error("quota"); } }, nav: NAV, loc: LOC, now: FIXED });
  feedback.open();
  doc.getElementById("bp-feedback-text").value = "hello";
  await feedback.submit();
  assert.equal(doc.getElementById("bp-feedback-status").textContent, "This report could not be saved. Try again.");
  assert.equal(doc.getElementById("bp-feedback-send").disabled, false);
  assert.equal(feedback.isOpen(), true);
});

test("without the logger the dialog explains and Send stays off", () => {
  const doc = makeDoc();
  const feedback = createFeedback({ doc, log: undefined, nav: NAV, loc: LOC });
  feedback.open();
  assert.equal(doc.getElementById("bp-feedback-send").disabled, true);
  assert.match(doc.getElementById("bp-feedback-status").textContent, /not available/);
});

test("Ctrl+Shift+F opens the dialog in the capture phase and is not forwarded", () => {
  const doc = makeDoc();
  const previous = globalThis.BPFeedback;
  try {
    const feedback = installFeedback(doc);
    const [{ fn, capture }] = doc.listeners.keydown;
    assert.equal(capture, true);
    assert.equal(globalThis.BPFeedback, feedback);
    const calls = [];
    const event = { ctrlKey: true, shiftKey: true, altKey: false, metaKey: false, code: "KeyF", key: "F",
      preventDefault: () => calls.push("prevent"), stopImmediatePropagation: () => calls.push("stopImmediate"), stopPropagation: () => calls.push("stop") };
    fn(event);
    assert.deepEqual(calls, ["prevent", "stopImmediate", "stop"]);
    assert.equal(feedback.isOpen(), true);
    feedback.close();
    const other = { ctrlKey: true, shiftKey: false, altKey: false, metaKey: false, code: "KeyF", key: "f", preventDefault: () => calls.push("no") };
    fn(other);
    assert.ok(!calls.includes("no"));
    assert.equal(feedback.isOpen(), false);
  } finally {
    globalThis.BPFeedback = previous;
  }
});

// ---- wiring ----------------------------------------------------------------------------------

test("the page links the logger first, the dialog styles, and the two entry points", () => {
  const index = readFileSync(new URL("../public/index.html", import.meta.url), "utf8");
  assert.ok(index.indexOf('<script src="/dk-log.js"></script>') > 0);
  assert.ok(index.indexOf("/dk-log.js") < index.indexOf("/dk-gate.css"), "the logger loads before styles and modules");
  assert.match(index, /<link rel="stylesheet" href="\/dk-feedback\.css">/);
  assert.match(index, /id="more-feedback"[^>]*data-ui-action="feedback"[\s\S]*?Report a problem/);
  assert.match(index, /id="settings-report-problem"[^>]*data-ui-action="feedback"/);
  const receiver = readFileSync(new URL("../public/receiver.js", import.meta.url), "utf8");
  assert.match(receiver, /feedback: \(\) => \{ popovers\.close\(\); globalThis\.BPFeedback\?\.open\(\); \}/);
  const app = readFileSync(new URL("../public/dk-app.js", import.meta.url), "utf8");
  assert.match(app, /installFeedback\(doc\)/);
});
