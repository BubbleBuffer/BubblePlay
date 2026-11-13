// The browser CI has no real Chromium, so the WSS worker path is covered by: a
// pure path selector, the worker client handshake against a fake Worker, and
// source contracts on receiver.js and the worker files. Each fails if the default
// reverts to main-thread decode or a policy of the old path is dropped.
import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { WSS_WORKER_DEFAULT, selectWssDecoderPath } from "../public/transport.js";
import { createWssWorkerClient, wssWorkerSupported } from "../public/wss-worker-client.js";

const read = (name) => readFileSync(new URL(`../public/${name}`, import.meta.url), "utf8");

test("WSS decodes in the worker by default when the browser supports it", () => {
  assert.equal(WSS_WORKER_DEFAULT, true);
  assert.equal(selectWssDecoderPath({ supported: true }), "worker");
  assert.equal(selectWssDecoderPath({ supported: true, requested: null }), "worker");
  // ?decoderPath=webcodecs is the WebRTC opt-in; it must not change the WSS choice.
  assert.equal(selectWssDecoderPath({ supported: true, requested: "webcodecs" }), "worker");
});

test("the main-thread path remains for unsupported browsers, the debug override and a worker failure", () => {
  assert.equal(selectWssDecoderPath({ supported: false }), "main");
  assert.equal(selectWssDecoderPath({ supported: true, requested: "main" }), "main");
  assert.equal(selectWssDecoderPath({ supported: true, disabled: true }), "main");
  assert.equal(selectWssDecoderPath({ supported: false, requested: "worker" }), "main");
  assert.equal(selectWssDecoderPath({ supported: true, requested: "worker", workerDefault: false }), "worker");
  assert.equal(selectWssDecoderPath({ supported: true, workerDefault: false }), "main");
  assert.equal(selectWssDecoderPath(), "main", "no capability information means the safe path");
});

test("feature detection needs Worker, VideoDecoder, OffscreenCanvas and transferControlToOffscreen", () => {
  const full = {
    Worker: function Worker() {}, VideoDecoder: function VideoDecoder() {}, OffscreenCanvas: function OffscreenCanvas() {},
    HTMLCanvasElement: { prototype: { transferControlToOffscreen() {} } },
  };
  assert.equal(wssWorkerSupported(full), true);
  for (const missing of ["Worker", "VideoDecoder", "OffscreenCanvas", "HTMLCanvasElement"]) {
    assert.equal(wssWorkerSupported({ ...full, [missing]: undefined }), false, missing);
  }
  assert.equal(wssWorkerSupported({ ...full, HTMLCanvasElement: { prototype: {} } }), false);
});

function fakeWorkerHarness({ supported = true, ready = true, transferThrows = false } = {}) {
  const sent = [];
  const worker = {
    terminated: false,
    postMessage(message, transfer) {
      sent.push({ message, transfer });
      if (message.type === "capabilities") queueMicrotask(() => worker.onmessage({ data: { type: "capabilities", supported } }));
      if (message.type === "init" && ready) queueMicrotask(() => worker.onmessage({ data: { type: "worker-ready" } }));
    },
    terminate() { this.terminated = true; },
  };
  const canvas = (name) => ({ transferControlToOffscreen() { if (transferThrows) throw new Error("no"); return { offscreen: name }; } });
  const messages = [];
  const client = createWssWorkerClient({ createWorker: () => worker, onMessage: (m) => messages.push(m), timeoutMs: 20 });
  return { client, worker, sent, messages, canvases: { main: canvas("main"), sharp: canvas("sharp") } };
}

test("the client transfers both canvases once the worker reports support", async () => {
  const h = fakeWorkerHarness();
  const result = await h.client.start({ canvases: h.canvases, epoch: 5, codec: "hevc", timeOrigin: 42 });
  assert.deepEqual(result, { ok: true, transferred: true });
  assert.equal(h.client.ready, true);
  const init = h.sent.find((s) => s.message.type === "init");
  assert.deepEqual(init.message.main, { offscreen: "main" });
  assert.deepEqual(init.message.sharp, { offscreen: "sharp" });
  assert.equal(init.message.epoch, 5);
  assert.equal(init.transfer.length, 2);
  h.client.post({ type: "frame" }, []);
  assert.equal(h.sent.at(-1).message.type, "frame");
  h.client.stop();
  assert.equal(h.worker.terminated, true);
  h.client.post({ type: "frame-after-stop" });
  assert.notEqual(h.sent.at(-1).message.type, "frame-after-stop");
});

test("an unsupported worker leaves the canvases with the page", async () => {
  const h = fakeWorkerHarness({ supported: false });
  const result = await h.client.start({ canvases: h.canvases, epoch: 0, codec: "hevc" });
  assert.deepEqual(result, { ok: false, reason: "worker-unsupported", transferred: false });
  assert.equal(h.worker.terminated, true);
});

test("a failed transfer or a silent worker falls back and reports whether canvases moved", async () => {
  const failed = fakeWorkerHarness({ transferThrows: true });
  assert.deepEqual(await failed.client.start({ canvases: failed.canvases, epoch: 0, codec: "hevc" }),
    { ok: false, reason: "transfer-failed", transferred: false });
  const silent = fakeWorkerHarness({ ready: false });
  const result = await silent.client.start({ canvases: silent.canvases, epoch: 0, codec: "hevc" });
  assert.equal(result.ok, false);
  assert.equal(result.reason, "worker-timeout");
  assert.equal(result.transferred, true, "the page must replace the canvases it already gave away");
  assert.equal(silent.worker.terminated, true);
});

test("messages after the handshake reach the receiver and a worker error is fatal", async () => {
  const fatal = [];
  const h = fakeWorkerHarness();
  const client = createWssWorkerClient({ createWorker: () => h.worker, onMessage: (m) => h.messages.push(m), onFatal: (r) => fatal.push(r) });
  await client.start({ canvases: h.canvases, epoch: 0, codec: "hevc" });
  h.worker.onmessage({ data: { type: "out", presented: true } });
  assert.deepEqual(h.messages, [{ type: "out", presented: true }]);
  h.worker.onerror({ message: "boom" });
  assert.deepEqual(fatal, ["worker-error:boom"]);
});

test("receiver.js picks the path with the selector and keeps the main-thread fallback", () => {
  const client = read("receiver.js");
  assert.match(client, /selectWssDecoderPath\(\{\s*requested: params\.get\("decoderPath"\),\s*supported: wssWorkerSupported\(\),\s*disabled: wssWorkerDisabled,/);
  assert.match(client, /if \(selectedWssDecoderPath\(\) === "worker"\) beginWssWorker\(\); else ensureMainCanvas\(\);/);
  // Worker failure and a missing worker both end on the main-thread path.
  assert.match(client, /function failWssWorker\(reason\)[\s\S]*wssWorkerDisabled = true;[\s\S]*stopWssMedia\(\);[\s\S]*setTimeout\(requestWssMediaStart, 0\)/);
  assert.match(client, /"wss-worker-unavailable"[\s\S]*teardownWssWorker\(\);\s*ensureMainCanvas\(\);/);
  // The canvases come back whenever WSS stops, and the host start waits for the handshake.
  assert.match(client, /function stopWssMedia\(\) \{\s*usingWssMedia = false;\s*teardownWssWorker\(\);/);
  assert.match(client, /Promise\.all\(\[selectWssCodec\(\), wssAudioDecodable, wssWorkerInit\]\)/);
  assert.match(client, /function teardownWssWorker\(\)[\s\S]*replaceTransferredCanvas\(\);\s*replaceTransferredSharpCanvas\(\);/);
});

test("ACKs stay on receipt and report the worker decodedSeq; recovery and watchdog stay in the receiver", () => {
  const client = read("receiver.js");
  assert.match(client, /ACK on receipt[\s\S]*wssSocket\.send\(ackMessageV4\(\{[^}]*decodedSeq: wssV4\.decodedSeq[\s\S]*?decodeWssFrame\(\{/);
  assert.match(client, /function onWssWorkerFrameOut\(m\)[\s\S]*wssV4\.decodedSeq = m\.decodedSequence/);
  assert.match(client, /function decodeWssFrame\(frame\) \{\s*if \(wssWorker\) \{\s*if \(wssWorker\.ready\) postWssFrameToWorker\(frame\);\s*return;/);
  assert.match(client, /function onWssWorkerMessage\(m\)[\s\S]*if \(m\.epoch !== wssDecoderEpoch\) return;/);
  assert.match(client, /case "failure":[\s\S]*noteWssFailure\(m\.reason\)/);
  assert.match(client, /case "drop":[\s\S]*beginWssRecovery\(m\.recover\)/);
  assert.match(client, /case "key-fed": armWssKeyframeWatchdog\(\)/);
  assert.match(client, /wssWorker\?\.post\(\{ type: "resync", epoch: wssDecoderEpoch, reason \}\)/);
});

test("the worker keeps the main-thread policies: low latency, immediate draw, desynchronized canvas", () => {
  const pipeline = read("wss-pipeline.js");
  assert.match(pipeline, /optimizeForLatency: true, hardwareAcceleration: "prefer-hardware"/);
  assert.match(pipeline, /desynchronized: true/);
  assert.doesNotMatch(pipeline, /requestAnimationFrame|setTimeout/, "frames are drawn in the decoder output callback");
  assert.match(pipeline, /shouldPresentDecodedV4/);
  assert.match(pipeline, /recover: "delta-before-keyframe"/);
  const worker = read("wss-worker.js");
  assert.match(worker, /createWssPipeline/);
  assert.match(worker, /case "capture"/);
  assert.match(worker, /case "tile-capability"/);
});

test("the main thread no longer reads or sizes the canvas on the worker path", () => {
  const client = read("receiver.js");
  assert.match(client, /function screenDims\(\)/);
  assert.match(client, /videoContentRect\(activeVideo \|\| screenMedia\(\)\)/);
  assert.match(client, /captureFrame\(\) \{\s*if \(wssWorker\) return captureFromWorker\(\);/);
  // Nothing in the worker-mode bookkeeping touches canvas size or context.
  const handler = client.slice(client.indexOf("function onWssWorkerFrameOut"), client.indexOf("function onTrack("));
  assert.doesNotMatch(handler, /screen\.(?:width|height)\s*=|getContext|drawImage|toDataURL/);
});
