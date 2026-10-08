import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { CLIPBOARD, clipboardPackets, ClipboardReceiver, InputController, keyDescriptor, packet, SCAN_CODES, WheelAccumulator } from "../public/input.js";

import { CLIPBOARD_CHUNK_TEXT_BYTES, CLIPBOARD_MAX_CHUNKS, validClipboardMessage, validInputCapabilities } from "../src/protocol-validation.js";

const contract =JSON.parse(readFileSync(new URL("../../fixtures/input-contract.json", import.meta.url), "utf8"));

function harness({ state = "open", accept = () => true } = {}) {
  const sent = [], timers = [];
  const env = { state, accept };
  const c = new InputController({
    sendControl: p => { if (!env.accept(p)) return false; sent.push([...p]); return true; },
    sendMotion: p => { sent.push([...p]); return true; },
    controlState: () => env.state,
    scheduleFlush: fn => timers.push(fn),
  });
  const runTimers = () => { while (timers.length) timers.shift()(); };
  return { c, sent, env, runTimers };
}

test("scan-code table matches the shared input contract fixture", () => {
  const table = Object.fromEntries(Object.entries(SCAN_CODES).map(([code, [scan, extended]]) => [code, { scan, extended }]));
  assert.deepEqual(table, contract.scanCodes);
  for (const code of contract.mustBeExtended) assert.equal(SCAN_CODES[code]?.[1], true, code);
  for (const [code, vk] of Object.entries(contract.vkOnly)) assert.deepEqual(keyDescriptor(code), { scan: 0, extended: false, vk, legacyVk: vk });
  // Left/right modifiers stay distinct.
  assert.notDeepEqual(SCAN_CODES.ControlLeft, SCAN_CODES.ControlRight);
  assert.notDeepEqual(SCAN_CODES.AltLeft, SCAN_CODES.AltRight);
  assert.notDeepEqual(SCAN_CODES.ShiftLeft, SCAN_CODES.ShiftRight);
  assert.equal(keyDescriptor("NoSuchKey"), null);
});

test("scan-key and clipboard packets match the contract bytes", () => {
  const [arrow, altgrUp, shift, pause, clip] = contract.packets;
  assert.deepEqual([...packet.scanKey(0x4b, true, 0x25, false)], arrow.bytes);
  assert.deepEqual([...packet.scanKey(0x38, true, 0xa5, true)], altgrUp.bytes);
  assert.deepEqual([...packet.scanKey(0x2a, false, 0xa0, false)], shift.bytes);
  assert.deepEqual([...packet.scanKey(0, false, 0x13, false)], pause.bytes);
  assert.deepEqual([...clipboardPackets("hé", 7)[0]], clip.bytes);
  assert.equal(contract.clipboardPacket.chunkBytes, CLIPBOARD.chunkBytes);
  assert.equal(contract.clipboardPacket.maxChunks, CLIPBOARD.maxChunks);
  assert.equal(contract.hostClipboardMessage.maxChunks, CLIPBOARD.hostMaxChunks);
});

test("physical keys use scan codes on capable hosts and legacy VKs otherwise", () => {
  const { c, sent } = harness();
  assert.equal(c.physicalKey("KeyA", false), true);
  assert.deepEqual(sent.at(-1), [0, 0x41, 0]);
  assert.equal(c.physicalKey("ControlRight", false), true);
  assert.deepEqual(sent.at(-1), [0, 0x11, 0], "legacy hosts keep generic modifier VKs");
  assert.equal(c.physicalKey("IntlBackslash", false), true);
  assert.equal(c.physicalKey("F24", false), true);
  c.scancode = true;
  c.physicalKey("AltRight", false);
  assert.deepEqual(sent.at(-1), [13, 2, 0x38, 0, 0xa5, 0]);
  c.physicalKey("AltRight", true);
  assert.deepEqual(sent.at(-1), [13, 3, 0x38, 0, 0xa5, 0]);
  c.physicalKey("Pause", false);
  assert.deepEqual(sent.at(-1), [13, 0, 0, 0, 0x13, 0]);
  assert.equal(c.physicalKey("Unidentified", false), false);
});

test("congested control is queued in order and never overtaken by motion", () => {
  const { c, sent, env, runTimers } = harness({ state: "busy" });
  c.physicalKey("KeyH", false); c.physicalKey("KeyH", true); c.buttons(1);
  c.motion(3, 4); assert.equal(c.flushMotion(), false, "motion waits behind queued control");
  assert.equal(sent.length, 0); assert.equal(c.controlBacklog, 3);
  env.state = "open"; runTimers();
  assert.deepEqual(sent.map(p => p[0]), [0, 1, 4, 7], "control flushes in order, then pending motion");
  assert.equal(c.stats.controlDropped, 0);
});

test("a dead transport discards control and resynchronizes with release-all first", () => {
  const { c, sent, env, runTimers } = harness({ state: "busy" });
  c.physicalKey("ShiftLeft", false);
  env.state = "down"; runTimers();
  assert.equal(c.controlBacklog, 0); assert.equal(c.stats.controlDropped, 1);
  assert.equal(c.physicalKey("KeyA", false), false, "nothing is queued into a dead socket");
  env.state = "open";
  c.physicalKey("KeyB", false);
  assert.deepEqual(sent, [[6], [0, 0x42, 0]]);
  assert.equal(c.stats.releaseResync, 1);
});

test("releaseAll without sending (gate closed) forces release-all before the next event", () => {
  const { c, sent } = harness();
  c.physicalKey("KeyA", false);
  c.releaseAll(false);
  c.physicalKey("KeyC", false);
  assert.deepEqual(sent, [[0, 0x41, 0], [6], [0, 0x43, 0]]);
});

test("queue overflow discards rather than growing without bound", () => {
  const c = new InputController({ sendControl: () => false, controlState: () => "busy", scheduleFlush: () => {}, maxQueuedBytes: 16 });
  for (let i = 0; i < 5; i++) c.key(65, false);
  assert.equal(c.controlBacklog, 5);
  c.key(65, true);
  assert.equal(c.controlBacklog, 0);
  assert.equal(c.needsRelease, true);
});

test("congested motion keeps wheel distance and final pointer position", () => {
  let buffered = 1000; const sent = [];
  const c = new InputController({ sendMotion: p => (sent.push([...p]), true), motionBuffered: () => buffered });
  c.motion(0, 0, 0, 60); assert.equal(c.flushMotion(), false);
  c.motion(0, 0, 0, 60); buffered = 0; assert.equal(c.flushMotion(), true);
  assert.deepEqual(sent[0].slice(7), [0, 0, 120, 0]);
  buffered = 1000; c.absolute(10, 20); c.flushMotion(); buffered = 0; c.flushMotion();
  assert.deepEqual(sent.at(-1), [8, 1, 0, 10, 0, 20, 0]);
});

test("wheel accumulator emits WHEEL_DELTA units and carries fractions", () => {
  const wheel = new WheelAccumulator();
  let total = 0;
  for (let i = 0; i < 10; i++) total += wheel.add(0, -3.3).y;
  assert.equal(total, 39, "10 x 3.3px = 33px = 39.6 units, 0.6 carried");
  assert.equal(wheel.add(0, -0.4).y, 1, "carried fraction completes the next unit");
  assert.deepEqual(new WheelAccumulator().add(0, 1, 1), { x: 0, y: -40 }, "one line = a third of a notch, down is negative");
  assert.deepEqual(new WheelAccumulator().add(0, 100), { x: 0, y: -120 });
  assert.deepEqual(new WheelAccumulator().add(50, 0), { x: 60, y: 0 });
  assert.deepEqual(new WheelAccumulator().add(0, -1, 2), { x: 0, y: 360 });
  assert.deepEqual(new WheelAccumulator().add(NaN, Infinity), { x: 0, y: 0 });
});

test("clipboard text is chunked on the reliable lane, bounded to 1 MiB", () => {
  const { c, sent } = harness();
  const text = "é".repeat(CLIPBOARD.chunkBytes); // 2 bytes each -> 2 chunks
  assert.equal(c.clipboard(text, 9), true);
  assert.equal(sent.length, 2);
  assert.deepEqual(sent[0].slice(0, 8), [12, 1, 9, 0, 0, 0, 2, 0]);
  assert.deepEqual(sent[1].slice(0, 8), [12, 1, 9, 0, 1, 0, 2, 0]);
  const joined = new TextDecoder().decode(Uint8Array.from([...sent[0].slice(8), ...sent[1].slice(8)]));
  assert.equal(joined, text);
  assert.ok(sent.every(p => p.length <= 16_384), "fits the relay's 16 KiB binary limit");
  assert.equal(clipboardPackets("", 1), null);
  assert.equal(clipboardPackets("x".repeat(CLIPBOARD.maxBytes + 1), 1), null);
  assert.equal(clipboardPackets("x".repeat(CLIPBOARD.maxBytes), 1).length, CLIPBOARD.maxChunks);
  assert.equal(clipboardPackets("hi", 1, { paste: false })[0][1], 0);
});

test("host clipboard chunks reassemble only in order", () => {
  const r = new ClipboardReceiver();
  assert.equal(r.push({ protocol: 1, id: 4, index: 0, count: 2, text: "ab" }), null);
  assert.equal(r.push({ protocol: 1, id: 4, index: 1, count: 2, text: "cd" }), "abcd");
  assert.equal(r.push({ protocol: 1, id: 5, index: 1, count: 2, text: "x" }), null, "missing first chunk");
  assert.equal(r.push({ protocol: 1, id: 6, index: 0, count: 1, text: "" }), "");
  assert.equal(r.push({ protocol: 2, id: 6, index: 0, count: 1, text: "x" }), null);
  assert.equal(r.push({ protocol: 1, id: 6, index: 0, count: 65, text: "x" }), null);
  r.push({ protocol: 1, id: 7, index: 0, count: 3, text: "a" });
  assert.equal(r.push({ protocol: 1, id: 8, index: 1, count: 3, text: "b" }), null, "interleaved transfer resets");
});

test("relay validates host clipboard chunks and input capabilities", () => {
  assert.equal(CLIPBOARD_CHUNK_TEXT_BYTES, contract.hostClipboardMessage.chunkBytes);
  assert.equal(CLIPBOARD_MAX_CHUNKS, contract.hostClipboardMessage.maxChunks);
  const ok = { type: "clipboard", protocol: 1, id: 3, index: 0, count: 1, text: "hé" };
  const check = (msg, role = "host") => validClipboardMessage(msg, role, JSON.stringify(msg));
  assert.equal(check(ok), true);
  assert.equal(check({ ...ok, text: "" }), true);
  assert.equal(check({ ...ok, text: "\u0001".repeat(CLIPBOARD_CHUNK_TEXT_BYTES) }), true, "escaped control chars fit");
  assert.equal(check(ok, "client"), false, "browsers cannot inject host clipboard messages");
  assert.equal(check({ ...ok, protocol: 2 }), false);
  assert.equal(check({ ...ok, index: 1 }), false);
  assert.equal(check({ ...ok, count: 0 }), false);
  assert.equal(check({ ...ok, count: CLIPBOARD_MAX_CHUNKS + 1 }), false);
  assert.equal(check({ ...ok, id: -1 }), false);
  assert.equal(check({ ...ok, text: 7 }), false);
  assert.equal(check({ ...ok, extra: true }), false);
  assert.equal(check({ ...ok, text: "é".repeat(CLIPBOARD_CHUNK_TEXT_BYTES / 2 + 1) }), false);

  const caps = { type: "input-capabilities", protocol: 1, scancode: true, clipboard: true, clipboardMaxBytes: CLIPBOARD.maxBytes };
  const capsOk = (msg, role = "host") => validInputCapabilities(msg, role, JSON.stringify(msg));
  assert.equal(capsOk(caps), true);
  assert.equal(capsOk(caps, "client"), false);
  assert.equal(capsOk({ ...caps, scancode: "yes" }), false);
  assert.equal(capsOk({ ...caps, clipboardMaxBytes: CLIPBOARD.maxBytes + 1 }), false);
  assert.equal(capsOk({ ...caps, extra: 1 }), false);
});

test("receiver wires physical keys, keyboard lock, wheel accumulation and clipboard", () => {
  const client = readFileSync(new URL("../public/receiver.js", import.meta.url), "utf8");
  const html = readFileSync(new URL("../public/index.html", import.meta.url), "utf8");
  assert.match(client, /controlState: inputControlState/);
  assert.match(client, /function inputControlState\(\) \{\s*if \(desktopInputState !== "enabled" \|\| !desktopAvailable \|\| desktopAwaitingFrame\) return "down";/);
  assert.match(client, /function releaseInput\(send = true\)/);
  assert.match(client, /input\.physicalKey\(e\.code, false\)/);
  assert.match(client, /input\.physicalKey\(e\.code, true\)/);
  // A held key resends its press on every browser repeat event.
  assert.match(client, /if \(held\.has\(e\.code\)\) \{[^}]*if \(e\.repeat\) input\.physicalKey\(e\.code, false\);\s*return;\s*\}/);
  assert.doesNotMatch(client, /const KEY = \{/);
  // Scroll speed and the keyboard-lock preference come from the saved session settings.
  assert.match(client, /wheelAccumulator\.add\(e\.deltaX \* settings\.scrollSpeed, e\.deltaY \* settings\.scrollSpeed, e\.deltaMode\)/);
  assert.match(client, /if \(document\.fullscreenElement && settings\.keyboardLock\) keyboard\.lock\(\)\.catch\(\(\) => \{\}\); else keyboard\.unlock\?\.\(\);/);
  assert.match(client, /keyboardLockSwitch\.onchange = \(\) => \{ settings\.keyboardLock = keyboardLockSwitch\.checked; persistSettings\(\); updateKeyboardLock\(\); \}/);
  assert.match(client, /document\.addEventListener\("fullscreenchange", updateKeyboardLock\)/);
  assert.match(client, /case "clipboard":\s*receiveRemoteClipboard\(msg\);/);
  assert.match(client, /case "host-capabilities":\s*applyInputCapabilities\(null\);/, "stale capabilities reset when a host (re)announces");
  assert.match(client, /clipboardSyncEnabled\(\) \{ return hostInputCapabilities\.clipboard && desktopKind !== "secure"/);
  assert.match(client, /remoteClipboardWriter\.receive\(content, document\.hasFocus\(\)\)/);
  assert.match(html, /<button id="remote-clipboard" type="button" hidden[^>]*>Copy remote clipboard<\/button>/);
});

test("desktop mouse drags stay captured and clamp to the picture edge", () => {
  const client = readFileSync(new URL("../public/receiver.js", import.meta.url), "utf8");
  // A held button keeps the pointer on the input surface even over the strip
  // or outside the browser window (edge hold needs the remote cursor at the edge).
  assert.match(client, /inputSurface\.addEventListener\("pointerdown", e => \{ if \(e\.pointerType === "mouse" && pointerMode === "desktop"\) \{ try \{ inputSurface\.setPointerCapture\(e\.pointerId\); \} catch \{\} \}/);
  // Absolute moves outside the picture (letterbox, outside the window) are clamped, not dropped.
  assert.match(client, /const x = clamp\(\(e\.clientX - r\.left\) \/ r\.width, 0, 1\);\s*const y = clamp\(\(e\.clientY - r\.top\) \/ r\.height, 0, 1\);/);
});

test("relay wires clipboard validation and replays input capabilities to new clients", () => {
  const source = readFileSync(new URL("../src/index.js", import.meta.url), "utf8");
  assert.match(source, /\(msg\.type === "clipboard" \|\| msg\.type === "clipboard-image"\) && !validClipboardMessage\(msg, att\.role, message\)\) return;/);
  assert.match(source, /validInputCapabilities\(msg, att\.role, message\)\) return;\s*ws\.serializeAttachment\(\{ \.\.\.ws\.deserializeAttachment\(\), inputCapabilities: message \}\)/);
  assert.match(source, /inputCapabilities = this\.findPeer\("host"\)\?\.deserializeAttachment\(\)\?\.inputCapabilities;\s*if \(typeof inputCapabilities === "string"\) ws\.send\(inputCapabilities\);/);
});
