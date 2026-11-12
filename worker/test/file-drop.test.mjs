import { recoveryStub } from "./helpers/recovery-stub.mjs";
import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { runInNewContext } from "node:vm";
import { decodeFilePacket, encodeOffer, validOfferMeta } from "../public/file-transfer.js";
import { createFileDragPreviewController, installFileTransferUi, largeDropStatus, LARGE_DROP_BYTES, mapFileDropPoint, positionFileDropBadge, statusText } from "../public/file-ui.js";
// receiver.js now reports to monitor-previews.js; these vm contexts do not exercise it.
const noMonitorPreviews = { reset() {}, onFirstFrame() {}, onMonitorList() {}, onMessage() {} };

const meta = { name: "a.txt", size: 3, mime: "text/plain", mtimeMs: 0, purpose: "drop", batchId: 1, batchIndex: 0, batchCount: 1, drop: { x: 65535, y: 0, generation: 7, viewportRevision: 3 } };
test("drop metadata requires bounded q16 coordinates; legacy purposes remain unchanged", () => {
  assert.deepEqual(decodeFilePacket(encodeOffer(1, meta)).json, meta);
  for (const drop of [null, undefined, { x: 65536, y: 0 }, { x: 0, y: -1 }, { x: 0.5, y: 0 }, { x: 0 }]) assert.equal(validOfferMeta({ ...meta, drop }), false);
  for (const stale of [{ generation: -1 }, { generation: 0 }, { generation: 2 ** 32 }, { viewportRevision: -1 }, { viewportRevision: 0.5 }]) assert.equal(validOfferMeta({ ...meta, drop: { ...meta.drop, ...stale } }), false);
  for (const purpose of ["save", "paste"]) {
    assert.equal(validOfferMeta({ ...meta, purpose }), false);
    const legacy = { ...meta, purpose }; delete legacy.drop;
    assert.deepEqual(decodeFilePacket(encodeOffer(1, legacy)).json, legacy);
  }
});

test("drop coordinates map visible full-desktop content through letterbox, zoom and pan", () => {
  const clip = { left: 0, top: 40, width: 1000, height: 600 };
  const content = { left: 0, top: 90, width: 1000, height: 500 };
  assert.deepEqual(mapFileDropPoint({ x: 500, y: 340 }, content, clip), { x: 32768, y: 32768 });
  assert.equal(mapFileDropPoint({ x: 500, y: 50 }, content, clip), null, "top letterbox is not host content");
  const zoomed = { left: -200, top: -60, width: 2000, height: 1000 };
  assert.deepEqual(mapFileDropPoint({ x: 500, y: 340 }, zoomed, clip), { x: 22937, y: 26214 });
  assert.equal(mapFileDropPoint({ x: 500, y: 20 }, zoomed, clip), null, "docked local UI clips zoomed content");
  assert.equal(mapFileDropPoint({ x: 500, y: 340 }, { ...content, width: 0 }, clip), null);
  assert.equal(mapFileDropPoint({ x: NaN, y: 340 }, content, clip), null);
});

test("native drop status waits for app acceptance and never claims clipboard save", () => {
  const row = { direction: "upload", purpose: "drop" };
  assert.match(statusText({ ...row, state: "verifying" }), /app to accept/);
  assert.equal(statusText({ ...row, state: "done" }), "Dropped on PC");
  assert.match(statusText({ ...row, state: "failed", error: "drop-rejected" }), /did not accept/);
  assert.match(statusText({ ...row, state: "failed", error: "drop-stale-view" }), /view changed/);
  assert.match(statusText({ ...row, state: "failed", error: "drop-desktop-unavailable" }), /desktop to return/);
  assert.match(statusText({ ...row, state: "failed", error: "drop-timeout" }), /too long/);
  assert.match(statusText({ ...row, state: "failed", error: "drop-input-busy" }), /Release the PC's keys and mouse buttons/);
  assert.match(statusText({ ...row, state: "failed", error: "drop-target-moved" }), /drop target changed.*Drop the files again/);
  assert.match(statusText({ ...row, state: "failed", error: "drop-target-changed" }), /drop target changed.*Drop the files again/);
  assert.match(statusText({ ...row, state: "failed", error: "drop-interrupted" }), /PC input changed.*Drop the files again/);
});

test("rejected presentation cannot replace native drop's accepted generation or geometry", () => {
  const receiver=readFileSync(new URL("../public/receiver.js",import.meta.url),"utf8");
  const start=receiver.indexOf("function markFirstFramePresented(");
  const end=receiver.indexOf("\nfunction handleFirstFrameMessage(",start);
  assert.ok(start>=0&&end>start);
  const source=receiver.slice(start,end);
  const present=(handshake,generation,revision)=>{
    const context={firstFrameHandshake:{presented:false,requireAdmission:true,admitted:true,generation:9,...handshake},
      fileDropPresentedGeneration:7,fileDropPresentedViewportRevision:3,usingWssMedia:true,wssStartupRecoveries:1,
      hasPresentedFrame:false,desktopAwaitingFrame:true,inputTransitionHold:false,suppressDesktopTransitionOverlay:true,
      monitorPreviews:{onFirstFrame(){}},recovery:recoveryStub(),document:{body:{classList:{add(){}}}},debugTrace(){},clearFirstFrameWatchdog(){},renderDesktopState(){},showContextHint(){}};
    context.result=runInNewContext(`${source}\nmarkFirstFramePresented(${generation},${revision})`,context);
    return context;
  };
  for(const rejected of [present({admitted:false},9,4),present({},8,4)]){
    assert.equal(rejected.result,false);
    assert.equal(rejected.fileDropPresentedGeneration,7);
    assert.equal(rejected.fileDropPresentedViewportRevision,3);
    assert.equal(rejected.desktopAwaitingFrame,true);
  }
  const accepted=present({},9,4);
  assert.equal(accepted.result,true);assert.equal(accepted.fileDropPresentedGeneration,9);assert.equal(accepted.fileDropPresentedViewportRevision,4);
  assert.equal(accepted.desktopAwaitingFrame,false);
  const ongoing=present({presented:true},10,5);
  assert.equal(ongoing.result,true);assert.equal(ongoing.fileDropPresentedGeneration,10);assert.equal(ongoing.fileDropPresentedViewportRevision,5);
});

test("file UI reads files only at drop and captures the event point for native drop", () => {
  const oldWindow = globalThis.window;
  const oldDocument = globalThis.document;
  const handlers = new Map();
  globalThis.window = { innerWidth: 1000, innerHeight: 700, addEventListener(type, handler) { handlers.set(type, handler); } };
  const element = () => ({ hidden: true, textContent: "", style: {}, dataset: {}, classList: { add() {} }, append() {}, addEventListener() {}, replaceChildren() {}, setAttribute() {}, getBoundingClientRect() { return { width: 252, height: 72 }; } });
  globalThis.document = { body: element(), hidden: false, createElement: element, addEventListener(type, handler) { handlers.set(`document:${type}`, handler); } };
  const parts = new Map();
  const root = { hidden: true, querySelector(selector) { if (!parts.has(selector)) parts.set(selector, element()); return parts.get(selector); } };
  const veilParts = new Map();
  let marker;
  const veil = { hidden: true, dataset: {}, append(value) { marker = value; }, querySelector(selector) { if (!veilParts.has(selector)) veilParts.set(selector, element()); return veilParts.get(selector); } };
  const sent = [], ordering = []; let reads = 0;
  const client = { offer: null, transfers: [], addEventListener() {}, upload(...args) { ordering.push("upload"); sent.push(args); }, rejectUpload(...args) { ordering.push("reject"); sent.push(args); } };
  const file = new File(["a"], "a.txt");
  const transfer = { types: ["Files"], items: [{ kind: "file", getAsFile() { reads++; return file; } }] };
  const event = { clientX: 123, clientY: 456, dataTransfer: transfer, preventDefault() {} };
  let choice = "drop";
  try {
    const ui = installFileTransferUi({ client, root, veil, onDragPreviewEnd: () => ordering.push("preview-end"), resolveDrop: event => choice === "drop"
      ? { purpose: "drop", drop: { x: event.clientX, y: event.clientY } }
      : choice === "error" ? { error: "drop-not-ready" } : { purpose: "save" } });
    ui.setEnabled(true);
    handlers.get("dragenter")(event); handlers.get("dragover")(event);
    assert.equal(reads, 0);
    assert.equal(veilParts.get("strong").textContent, "Drop on PC here");
    assert.match(veilParts.get("span").textContent, /1 file.*Release to send to this spot/);
    assert.equal(veil.dataset.mode, "preview");
    assert.deepEqual(marker.style, { left: "123px", top: "456px" });
    assert.deepEqual(veilParts.get("div").style, { left: "147px", top: "480px" });
    assert.equal(transfer.dropEffect, "copy");
    handlers.get("dragenter")(event);
    handlers.get("dragleave")({ ...event, relatedTarget: {}, dataTransfer: { types: [] } });
    assert.equal(veil.hidden, false, "nested leave with protected data retains preview");
    handlers.get("dragleave")({ ...event, clientX: 0, relatedTarget: null, dataTransfer: { types: [] } });
    assert.equal(veil.hidden, true, "empty types on window exit still clears preview");
    for (const type of ["blur", "dragend", "resize"]) {
      handlers.get("dragenter")(event); handlers.get(type)(event);
      assert.equal(veil.hidden, true, `${type} clears preview`);
    }
    handlers.get("dragenter")(event);
    document.hidden = true; handlers.get("document:visibilitychange")(); document.hidden = false;
    assert.equal(veil.hidden, true);
    handlers.get("dragenter")(event); handlers.get("keydown")({ key: "Escape" }); handlers.get("dragover")(event);
    assert.equal(veil.hidden, true, "Escape suppresses the cancelled drag preview");
    handlers.get("dragenter")(event);
    event.clientX = 321;
    handlers.get("drop")(event);
    assert.equal(reads, 1);
    assert.deepEqual(sent, [[[file], "drop", { x: 321, y: 456 }]]);
    assert.deepEqual(ordering.slice(-2), ["preview-end", "upload"], "host hover ends before real upload starts");
    assert.equal(veil.hidden, true);
    choice = "save"; handlers.get("dragenter")(event);
    assert.equal(veil.dataset.mode, "save"); assert.equal(marker.hidden, true);
    assert.match(veilParts.get("span").textContent, /Downloads/);
    handlers.get("drop")(event);
    assert.deepEqual(sent.at(-1), [[file], "save", undefined], "outside-stream/legacy callback keeps save");
    choice = "error"; handlers.get("dragenter")(event);
    assert.equal(veil.dataset.state, "blocked"); assert.equal(transfer.dropEffect, "none");
    assert.equal(veilParts.get("strong").textContent, "Can't drop here yet");
    assert.equal(reads, 2, "blocked preview still does not read files");
    handlers.get("drop")(event);
    assert.deepEqual(sent.at(-1), [[file], "drop", "drop-not-ready"], "stale geometry does not silently save");
    handlers.get("dragenter")(event); ui.setEnabled(false); handlers.get("drop")(event);
    assert.equal(veil.hidden, true, "disable clears preview");
    assert.equal(sent.length, 3, "capability loss prevents file submission");
  } finally { globalThis.window = oldWindow; globalThis.document = oldDocument; }
});

test("host drag preview sends metadata only, throttles updates and keeps a stationary hover alive", () => {
  const sent = [], timers = new Map(); let clock = 0, available = true, epoch = "a".repeat(32), stale = false;
  const point = event => stale ? { error: "drop-not-ready" } : { purpose: "drop", drop: { x: event.clientX, y: event.clientY, generation: 7, viewportRevision: 3 } };
  const controller = createFileDragPreviewController({ send: message => sent.push(message), getContext: () => ({ available, epoch }), resolveDrop: point,
    now: () => clock, every: (fn, delay) => { assert.equal(delay, 500); timers.set(1, fn); return 1; }, stop: id => timers.delete(id) });
  const event = { clientX: 123, clientY: 456, dataTransfer: { items: [{ kind: "file", type: "image/png", getAsFile() { throw Error("protected file read"); }, get name() { throw Error("protected name read"); } }, { kind: "string", type: "text/plain" }] } };
  controller.update(event);
  assert.equal(sent.length, 1); assert.equal(sent[0].action, "update"); assert.ok(sent[0].dragId > 0);
  assert.deepEqual(sent[0].types, ["image/png"]); assert.deepEqual(sent[0].drop, { x: 123, y: 456, generation: 7, viewportRevision: 3 });
  event.clientX = 321; clock = 100; controller.update(event); assert.equal(sent.length, 1);
  clock = 200; controller.update(event); assert.equal(sent.length, 2); assert.equal(sent[1].drop.x, 321); assert.equal(sent[0].drop.x, 123);
  clock = 700; timers.get(1)(); assert.equal(sent.length, 3); assert.equal(sent[2].dragId, sent[0].dragId);
  stale = true; clock = 1200; timers.get(1)(); assert.equal(timers.size, 0);
  assert.deepEqual(sent.at(-1), { type: "file-drag-preview", protocol: 1, epoch, dragId: sent[0].dragId, action: "cancel" });
  stale = false; controller.update(event); assert.notEqual(sent.at(-1).dragId, sent[0].dragId);
  available = false; controller.update(event); assert.equal(sent.at(-1).action, "cancel"); assert.equal(timers.size, 0);
  const length = sent.length; controller.update(event); assert.equal(sent.length, length, "unsupported capability sends nothing");
  available = true; controller.update(event); epoch = "b".repeat(32); timers.get(1)();
  assert.equal(timers.size, 0); assert.equal(sent.at(-1).epoch, "a".repeat(32), "no old drag is replayed into a new epoch");
  controller.cancel();
});

test("large drop status aggregates ACKs including empty files and waits for every host result", () => {
  const rows = [{ size: LARGE_DROP_BYTES, bytes: LARGE_DROP_BYTES / 2, state: "active" }, { size: 0, bytes: 0, state: "queued" }];
  assert.equal(largeDropStatus(rows).title, "Sending files to PC… 50%");
  rows[0].bytes = LARGE_DROP_BYTES; rows[0].state = "verifying";
  assert.equal(largeDropStatus(rows).state, "sending", "an empty queued file still needs staging");
  rows[1].state = "verifying";
  assert.equal(largeDropStatus(rows).title, "Attaching…");
  rows[0].state = "done";
  assert.equal(largeDropStatus(rows).title, "Attaching…", "one successful result cannot complete a batch");
  rows[1].state = "done";
  assert.equal(largeDropStatus(rows).title, "Dropped on PC");
  rows[1].state = "failed"; rows[1].error = "drop-rejected";
  assert.equal(largeDropStatus(rows).state, "failed"); assert.notEqual(largeDropStatus(rows).title, "Dropped on PC");
  rows[1].state = "cancelled";
  assert.equal(largeDropStatus(rows).title, "File drop cancelled");
});

test("large native drop badge tracks returned IDs, preserves cancellation across drags and clears unsafe/reset state", () => {
  const oldWindow = globalThis.window, oldDocument = globalThis.document;
  const handlers = new Map(); let progress, changed, reads = 0, nextId = 1, stale = false, choice = "drop", ui;
  const element = () => ({ hidden: true, textContent: "", style: {}, dataset: {}, children: [], classList: { add() {} },
    append(...children) { this.children.push(...children); }, addEventListener() {}, replaceChildren() {}, setAttribute() {},
    getBoundingClientRect() { return { width: 288, height: 100 }; } });
  globalThis.window = { innerWidth: 1000, innerHeight: 700, addEventListener(type, handler) { handlers.set(type, handler); } };
  globalThis.document = { body: { append(node) { progress = node; } }, createElement: element, addEventListener() {} };
  const parts = new Map(), root = { querySelector(key) { if (!parts.has(key)) parts.set(key, element()); return parts.get(key); } };
  const veil = { ...element(), querySelector: root.querySelector };
  const cancelled = [];
  const client = { offer: null, transfers: [], addEventListener(type, handler) { changed = handler; },
    upload(files, purpose) { const ids = files.map(file => { const id = nextId++; this.transfers.push({ id, purpose, direction: "upload", name: file.name, size: file.size, bytes: 0, state: "queued" }); return id; }); changed(); return ids; },
    cancel(id) { cancelled.push(id); }, dismiss() {} };
  const drop = sizes => { const event = { clientX: 123, clientY: 456, preventDefault() {}, dataTransfer: { types: ["Files"], items: sizes.map((size, index) => ({ kind: "file", getAsFile() { reads++; return { name: `${index}.bin`, size }; } })) } }; handlers.get("dragenter")(event); assert.equal(reads, 0); handlers.get("drop")(event); reads = 0; };
  try {
    ui = installFileTransferUi({ client, root, veil, resolveDrop: () => stale ? { error: "drop-stale-view" } : { purpose: choice, drop: meta.drop } }); ui.setEnabled(true);
    drop([LARGE_DROP_BYTES - 1]); assert.equal(progress.hidden, true, "below threshold retains small drop behavior");
    choice = "save"; drop([LARGE_DROP_BYTES]); assert.equal(progress.hidden, true, "save uploads never acquire this badge"); choice = "drop";
    drop([LARGE_DROP_BYTES / 2, 0, LARGE_DROP_BYTES / 2]); assert.equal(progress.hidden, false);
    client.transfers[0].state = "failed"; client.transfers[0].error = "disconnected";
    const selected = client.transfers.slice(2); selected[0].bytes = selected[0].size; selected[0].state = "verifying"; changed();
    assert.equal(progress.children[0].textContent, "Sending files to PC… 50%", "unrelated failures and bytes are excluded");
    handlers.get("dragenter")({ clientX: 200, clientY: 300, preventDefault() {}, dataTransfer: { types: ["Files"], items: [] } }); assert.equal(progress.hidden, true);
    handlers.get("dragleave")({ clientX: 0, clientY: 0, relatedTarget: null }); assert.equal(progress.hidden, false);
    progress.children[2].onclick({ stopPropagation() {} }); assert.deepEqual(cancelled, [selected[0].id], "client cancels the selected native batch once");
    for (const row of selected) { row.bytes = row.size; row.state = "verifying"; } changed(); assert.equal(progress.children[0].textContent, "Attaching…");
    for (const row of selected) row.state = "cancelled"; changed(); assert.equal(progress.children[0].textContent, "File drop cancelled");
    progress.children[2].onclick({ stopPropagation() {} }); assert.equal(progress.hidden, true);
    drop([LARGE_DROP_BYTES]); stale = true; changed(); assert.equal(progress.hidden, true, "stale coordinates remove the anchored badge"); stale = false;
    drop([LARGE_DROP_BYTES]); client.transfers.at(-1).state = "failed"; client.transfers.at(-1).error = "disconnected"; changed(); assert.equal(progress.hidden, true, "epoch reset hides the batch");
    drop([LARGE_DROP_BYTES]); ui.setEnabled(false); assert.equal(progress.hidden, true);
  } finally { ui?.setEnabled(false); globalThis.window = oldWindow; globalThis.document = oldDocument; }
});

test("host preview bounds MIME hints and encoded control bytes; save route cancels it", () => {
  const sent = []; let timer;
  const target = { purpose: "drop", drop: meta.drop };
  const controller = createFileDragPreviewController({ send: message => sent.push(message), getContext: () => ({ available: true, epoch: "a".repeat(32) }), resolveDrop: () => target,
    now: () => 0, every: fn => { timer = fn; return 1; }, stop: () => { timer = null; } });
  controller.update({ dataTransfer: { items: [] } }, target); assert.deepEqual(sent[0].types, [""]);
  controller.cancel();
  controller.update({ dataTransfer: { items: [{ kind: "file", type: "é" }, { kind: "file", type: "text/plain invalid" }, { kind: "file", type: "text/\0plain" }, { kind: "file", type: "image/png" }] } }, target);
  assert.deepEqual(sent.at(-1).types, ["", "", "", "image/png"]);
  controller.cancel();
  controller.update({ dataTransfer: { items: Array.from({ length: 80 }, () => ({ kind: "file", type: "\\".repeat(96) })) } }, target);
  assert.equal(sent.at(-1).types.length, 64); assert.ok(new TextEncoder().encode(JSON.stringify(sent.at(-1))).length <= 8192);
  controller.update({}, { purpose: "save" }); assert.equal(sent.at(-1).action, "cancel"); assert.equal(timer, null);
});

test("native badge flips at edges, clamps on narrow views, and never covers the drop point", () => {
  const size = { width: 252, height: 72 };
  assert.deepEqual(positionFileDropBadge({ x: 990, y: 690 }, size, { width: 1000, height: 700 }), { x: 714, y: 594 });
  for (const viewport of [{ width: 1440, height: 900 }, { width: 320, height: 640 }]) {
    for (const point of [{ x: 0, y: 0 }, { x: viewport.width - 1, y: 1 }, { x: viewport.width - 1, y: viewport.height - 1 }, { x: viewport.width / 2, y: viewport.height / 2 }]) {
      const badge = positionFileDropBadge(point, size, viewport);
      assert.ok(badge);
      assert.ok(badge.x >= 8 && badge.y >= 8 && badge.x + size.width <= viewport.width - 8 && badge.y + size.height <= viewport.height - 8);
      assert.ok(point.x < badge.x || point.x > badge.x + size.width || point.y < badge.y || point.y > badge.y + size.height);
    }
  }
  assert.equal(positionFileDropBadge({ x: 20, y: 20 }, size, { width: 100, height: 100 }), null);
});
