import test from "node:test";
import assert from "node:assert/strict";
import { ClipboardReceiver, CLIPBOARD_IMAGE, InputController } from "../public/input.js";
import { RemoteClipboardWriter, sameClipboardImage, pasteHostClipboard } from "../public/remote-clipboard.js";
import { validClipboardMessage } from "../src/protocol-validation.js";

const png = Uint8Array.from(Buffer.from("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==", "base64"));
const message = text => ({ type: "clipboard-image", protocol: 1, id: 4, index: 0, count: 1, mimeType: "image/png", text });
const tick = () => new Promise(resolve => setImmediate(resolve));

test("native PNG chunks reach browser as an image and relay admits only host PNG", () => {
  const m = message(Buffer.from(png).toString("base64"));
  const received = new ClipboardReceiver().push(m);
  assert.equal(received.mimeType, "image/png");
  assert.deepEqual(received.data, png);
  assert.equal(validClipboardMessage(m, "host", JSON.stringify(m)), true);
  assert.equal(validClipboardMessage(m, "client", JSON.stringify(m)), false);
  assert.equal(validClipboardMessage({ ...m, mimeType: "image/svg+xml" }, "host", JSON.stringify(m)), false);
  assert.equal(validClipboardMessage({ ...m, count: CLIPBOARD_IMAGE.maxChunks + 1 }, "host", JSON.stringify(m)), false);
  assert.equal(new ClipboardReceiver().push({ ...m, text: "AA==" }), null);
});

test("image and text chunks cannot be spliced; chunk bytes are bounded", () => {
  const r = new ClipboardReceiver(), base64 = Buffer.from(png).toString("base64");
  assert.equal(r.push({ ...message(base64.slice(0, 32)), count: 2 }), null);
  assert.equal(r.push({ type: "clipboard", protocol: 1, id: 4, index: 1, count: 2, text: "stale" }), null);
  assert.equal(r.push({ ...message("A".repeat(32769)) }), null);
  const oversized = png.slice(); new DataView(oversized.buffer).setUint32(16, 0xffffffff);
  assert.equal(r.push(message(Buffer.from(oversized).toString("base64"))), null);
});

test("browser image permission denial preserves the pending chip and click retry", async () => {
  const calls = [], pending = [];
  let allow = false;
  class Item { constructor(value) { this.value = value; } }
  const writer = new RemoteClipboardWriter({ ClipboardItem: Item, onPending: value => pending.push(value),
    clipboard: { write: items => { calls.push(items); return allow ? Promise.resolve() : Promise.reject(new Error("permission")); } } });
  const content = { mimeType: "image/png", data: png };
  writer.receive(content, true); await tick();
  assert.equal(writer.pending, content);
  assert.equal(calls[0][0].value["image/png"].type, "image/png");
  assert.deepEqual(new Uint8Array(await calls[0][0].value["image/png"].arrayBuffer()), png);
  allow = true; writer.write(); await tick();
  assert.equal(writer.pending, null);
  assert.equal(writer.latest, content);
  assert.equal(pending.at(-1), false);
});

test("writes serialize and an older completion never clears the latest image", async () => {
  const calls = [], finish = [];
  class Item { constructor(value) { this.value = value; } }
  const writer = new RemoteClipboardWriter({ ClipboardItem: Item, onPending() {}, clipboard: {
    writeText: value => { calls.push(value); return new Promise(resolve => finish.push(resolve)); },
    write: items => { calls.push(items); return new Promise(resolve => finish.push(resolve)); },
  } });
  writer.receive("old", true);
  const image = { mimeType: "image/png", data: png }; writer.receive(image, true);
  assert.equal(calls.length, 1);
  finish.shift()(); await tick();
  assert.equal(calls.length, 2); assert.equal(writer.pending, image);
  finish.shift()(); await tick(); assert.equal(writer.pending, null);
  writer.receive("queued", false); writer.clear(); assert.equal(writer.latest, null);
});

test("remote image paste recognizes exact and browser-reencoded copies while distinguishing local images", async () => {
  const content = { mimeType: "image/png", data: png };
  assert.equal(await sameClipboardImage(content, new Blob([png], { type: "image/png" })), true);
  const sanitized = new Blob([png, new Uint8Array([0])], { type: "image/png" });
  assert.equal(await sameClipboardImage(content, sanitized, async () => "1x1:same-rgba"), true);
  let counter = 0;
  assert.equal(await sameClipboardImage(content, sanitized, async () => String(counter++)), false);
  assert.equal(await sameClipboardImage(content, new Blob(["local"], { type: "text/plain" })), false);
});

test("async image paste sends a full Windows Ctrl+V and restores held client modifiers", () => {
  const pressed = new Set(["MetaLeft", "ControlRight"]), calls = [];
  pasteHostClipboard({ physicalKey(code, up) {
    calls.push([code, up]);
    if (up) pressed.delete(code); else pressed.add(code);
    if (code === "KeyV" && !up) {
      assert.equal(pressed.has("ControlLeft"), true);
      assert.equal(pressed.has("MetaLeft"), false);
      assert.equal(pressed.has("ControlRight"), false);
    }
  } }, new Set(pressed));
  assert.deepEqual(pressed, new Set(["MetaLeft", "ControlRight"]));
  assert.equal(calls.filter(([code]) => code === "KeyV").length, 2);
});

test("native paste uses ordered reliable key packets with and without a held Ctrl", () => {
  const packets = [], input = new InputController({ sendControl: packet => { packets.push([...packet]); return true; } });
  pasteHostClipboard(input);
  assert.deepEqual(packets, [[0,17,0], [0,86,0], [1,86,0], [1,17,0]]);
  packets.length = 0;
  pasteHostClipboard(input, new Set(["ControlLeft"]));
  assert.deepEqual(packets, [[1,17,0], [0,17,0], [0,86,0], [1,86,0], [1,17,0], [0,17,0]]);
});
