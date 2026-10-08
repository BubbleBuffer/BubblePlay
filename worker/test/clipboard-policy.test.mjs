import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { runInNewContext } from "node:vm";
import { ClipboardReceiver } from "../public/input.js";
import { RemoteClipboardWriter, pasteHostClipboard } from "../public/remote-clipboard.js";

const tick = () => new Promise(resolve => setImmediate(resolve));
const receiver = readFileSync(new URL("../public/receiver.js", import.meta.url), "utf8");

function writerHarness(autoCopy = true) {
  const calls = [], completions = [], pending = [];
  const writer = new RemoteClipboardWriter({ autoCopy, onPending: value => pending.push(value), clipboard: {
    writeText: value => { calls.push(value); return new Promise(resolve => completions.push(resolve)); },
  } });
  return { writer, calls, completions, pending };
}

test("manual mode keeps only the latest PC snapshot armed across focus until a click", async () => {
  const { writer, calls, completions, pending } = writerHarness(false);
  writer.receive("old", true); writer.receive("latest", true);
  writer.writeAutomatically(false); writer.writeAutomatically(true);
  assert.deepEqual(calls, []);
  assert.equal(writer.pending, "latest"); assert.equal(pending.at(-1), true);
  writer.write(); assert.deepEqual(calls, ["latest"]);
  completions.shift()(); await tick();
  assert.equal(writer.pending, null); assert.equal(pending.at(-1), false);
});

test("turning automatic copying off suppresses a newer snapshot queued behind an active write", async () => {
  const { writer, calls, completions } = writerHarness();
  writer.receive("submitted", true); writer.receive("queued", true);
  writer.setAutomatic(false, true);
  completions.shift()(); await tick();
  assert.deepEqual(calls, ["submitted"]); assert.equal(writer.pending, "queued");
  writer.write(); assert.deepEqual(calls, ["submitted", "queued"]);
  completions.shift()(); await tick();
});

test("new PC content in manual mode does not flush when an older automatic write finishes", async () => {
  const { writer, calls, completions } = writerHarness();
  writer.receive("submitted", true); writer.setAutomatic(false, true); writer.receive("new", true);
  completions.shift()(); await tick();
  assert.deepEqual(calls, ["submitted"]); assert.equal(writer.pending, "new");
  writer.setAutomatic(true, true); assert.deepEqual(calls, ["submitted", "new"]);
  completions.shift()(); await tick();
});

test("blur defers a queued automatic write until focus returns", async () => {
  const { writer, calls, completions } = writerHarness();
  writer.receive("submitted", true); writer.receive("queued", true); writer.writeAutomatically(false);
  completions.shift()(); await tick();
  assert.deepEqual(calls, ["submitted"]);
  writer.writeAutomatically(true); assert.deepEqual(calls, ["submitted", "queued"]);
  completions.shift()(); await tick();
});

test("local paste discards a deferred PC snapshot and prevents its queued write", async () => {
  const { writer, calls, completions, pending } = writerHarness();
  writer.receive("submitted", true); writer.receive("queued", true); writer.discardPending();
  completions.shift()(); await tick(); writer.writeAutomatically(true); writer.write();
  assert.deepEqual(calls, ["submitted"]); assert.equal(writer.pending, null);
  assert.equal(writer.latest, null); assert.equal(pending.at(-1), false);
});

function receiverHarness({ autoCopy = false, clipboard = {}, compare = async () => true } = {}) {
  const documentEvents = new Map(), windowEvents = new Map(), sent = [], uploads = [], button = { hidden: true }, setting = {};
  const context = {
    ClipboardReceiver, RemoteClipboardWriter, pasteHostClipboard, sameClipboardImage: compare,
    navigator: { clipboard }, globalThis: {}, settings: { clipboardAutoCopy: autoCopy },
    document: { hasFocus: () => true, getElementById: id => id === "setting-clipboard-auto-copy" ? setting : button,
      addEventListener: (name, handler) => documentEvents.set(name, handler) },
    window: { addEventListener: (name, handler) => windowEvents.set(name, handler) },
    desktopKind: "default", inputReady: true, mediaEpoch: 1, inputSurface: { focus() {} }, held: new Set(),
    keyboardActive: () => true, keyDescriptor: () => true,
    input: { physicalKey: (...args) => sent.push(args), clipboard: text => { sent.push(text); return true; } },
    fileTransferAvailable: () => true, filesFromPaste: event => event.files ?? [],
    fileClient: { upload: (...args) => uploads.push(args) },
    setTimeout, clearTimeout, persistSettings: () => { context.saved = { ...context.settings }; },
  };
  const clipboardBlock = receiver.slice(receiver.indexOf("let hostInputCapabilities ="), receiver.indexOf("// ---- file transfer (hook 5/5)"));
  const keydownStart = receiver.indexOf('document.addEventListener("keydown", (e) => {', receiver.indexOf("function keyboardActive()"));
  const keydownBlock = receiver.slice(keydownStart, receiver.indexOf('document.addEventListener("keyup",', keydownStart));
  const settingStart = receiver.indexOf('const clipboardAutoCopySwitch =');
  const settingBlock = receiver.slice(settingStart, receiver.indexOf('const scrollSpeedSelect =', settingStart));
  runInNewContext(`${clipboardBlock}\n${keydownBlock}\n${settingBlock}\nthis.writer = remoteClipboardWriter; this.receive = receiveRemoteClipboard; applyInputCapabilities({ protocol: 1, clipboard: true });`, context);
  const key = (key = "v", modifier = "ctrlKey") => {
    let prevented = false;
    documentEvents.get("keydown")({ key, code: `Key${key.toUpperCase()}`, [modifier]: true, preventDefault() { prevented = true; } });
    return prevented;
  };
  const paste = (text = "local", files = []) => documentEvents.get("paste")({ files, clipboardData: { getData: () => text }, preventDefault() {} });
  return { context, writer: context.writer, receive: context.receive, key, paste, documentEvents, windowEvents, sent, uploads, button, setting };
}

test("receiver Settings opt-out persists and host updates arm the chip until clicked", async () => {
  const writes = [], h = receiverHarness({ autoCopy: true, clipboard: { writeText: async text => { writes.push(text); } } });
  assert.equal(h.setting.checked, true);
  h.setting.checked = false; h.setting.onchange();
  assert.equal(h.context.saved.clipboardAutoCopy, false);
  h.receive({ protocol: 1, id: 1, index: 0, count: 1, text: "PC text" });
  h.windowEvents.get("focus")();
  assert.deepEqual(writes, []); assert.equal(h.button.hidden, false);
  h.button.onclick({ stopPropagation() {} }); await tick();
  assert.deepEqual(writes, ["PC text"]); assert.equal(h.button.hidden, true);
});

test("receiver automatic mode writes host text by default", async () => {
  const writes = [], h = receiverHarness({ autoCopy: true, clipboard: { writeText: async text => { writes.push(text); } } });
  h.receive({ protocol: 1, id: 1, index: 0, count: 1, text: "PC text" }); await tick();
  assert.deepEqual(writes, ["PC text"]); assert.equal(h.button.hidden, true);
});

test("Ctrl and Cmd paste use local text even when a host snapshot is pending", () => {
  for (const modifier of ["ctrlKey", "metaKey"]) {
    const h = receiverHarness();
    h.writer.receive("PC pending", true);
    assert.equal(h.key("v", modifier), false, "browser must raise the local paste event");
    assert.equal(h.writer.pending, null); assert.equal(h.button.hidden, true);
    h.paste("local text"); assert.deepEqual(h.sent, ["local text"]);
  }
});

test("copy, cut and their local keyboard shortcuts invalidate pending PC snapshots", () => {
  const h = receiverHarness();
  for (const key of ["c", "x"]) {
    h.writer.receive("PC pending", true); h.key(key);
    assert.equal(h.writer.pending, null); assert.equal(h.writer.latest, null);
  }
  for (const event of ["copy", "cut"]) {
    h.writer.receive("PC pending", true); h.documentEvents.get(event)();
    assert.equal(h.writer.pending, null); assert.equal(h.button.hidden, true);
  }
});

test("a not-yet-written PC image cannot substitute for a local pasted image", () => {
  let comparisons = 0;
  const h = receiverHarness({ compare: async () => { comparisons++; return true; } });
  const image = { mimeType: "image/png", data: new Uint8Array([1]) }, localFile = { type: "image/png" };
  h.writer.receive(image, true); h.key(); h.paste("", [localFile]);
  assert.equal(comparisons, 0); assert.deepEqual(h.uploads[0], [[localFile], "paste"]);
  assert.deepEqual(h.sent, []);
});

test("a successfully copied PC image still uses native host paste when unchanged", async () => {
  const h = receiverHarness({ autoCopy: true, clipboard: { writeText: async () => {} } });
  const image = { mimeType: "image/png", data: new Uint8Array([1]) }, localFile = { type: "image/png" };
  h.writer.ClipboardItem = class {}; h.writer.clipboard.write = async () => {};
  h.writer.receive(image, true); await tick(); h.key(); h.paste("", [localFile]); await tick();
  assert.equal(h.uploads.length, 0); assert.ok(h.sent.some(([code]) => code === "KeyV"));
});

test("PC replacement during image comparison still uploads the captured local paste", async () => {
  let resolveComparison;
  const h = receiverHarness({ autoCopy: true, compare: () => new Promise(resolve => { resolveComparison = resolve; }) });
  const image = { mimeType: "image/png", data: new Uint8Array([1]) }, localFile = { type: "image/png" };
  h.writer.ClipboardItem = class {}; h.writer.clipboard.write = async () => {};
  h.writer.receive(image, true); await tick(); h.key(); h.paste("", [localFile]);
  h.writer.receive("replacement", false); resolveComparison(true); await tick();
  assert.deepEqual(h.uploads[0], [[localFile], "paste"]); assert.deepEqual(h.sent, []);
});
