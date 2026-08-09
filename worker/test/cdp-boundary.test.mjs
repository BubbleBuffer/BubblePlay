import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

test("live collector attaches to a page target and collects rendered and barcode latency", () => {
  const source = readFileSync(new URL("../tools/latency-live.mjs", import.meta.url), "utf8");
  assert.match(source, /webSocketDebuggerUrl/);
  assert.match(source, /json\/list/);
  assert.match(source, /__bubbleplayDiagnostics/);
  assert.match(source, /750/);
  assert.match(source, /glassToGlassSamples/);
  assert.match(source, /timestamp-pattern\.html/);
  assert.doesNotMatch(source, /glassToGlass: \{ status: "unavailable"/);
});

test("modern transform keeps canvas/decode/render in the DedicatedWorker", () => {
  const client = readFileSync(new URL("../public/index.html", import.meta.url), "utf8");
  const worker = readFileSync(new URL("../public/latency-worker.js", import.meta.url), "utf8");
  assert.match(client, /message\.kind === "control"/);
  assert.match(client, /transferControlToOffscreen\(\)/);
  assert.match(client, /type: "canvas", canvas: offscreen/);
  assert.match(client, /type: "capabilities"/);
  assert.match(worker, /typeof VideoDecoder !== "undefined"/);
  assert.match(worker, /typeof OffscreenCanvas !== "undefined"/);
  assert.match(client, /jitterBufferTarget\s*=\s*0/);
  assert.doesNotMatch(client, /frame-consumed/);
  assert.doesNotMatch(client, /activeReceiver\.sendKeyFrameRequest/);
  assert.match(worker, /transformer\.sendKeyFrameRequest/);
  assert.match(worker, /transformer\.writable\.getWriter/);
  assert.match(worker, /await writer\.write\(value\)/);
  assert.match(worker, /new VideoDecoder/);
  assert.match(worker, /createWorkerTimeline/);
  // Backpressure is the WebCodecs control queue, not frames already accepted
  // by the hardware decoder and awaiting output.
  assert.match(worker, /MAX_DECODE_QUEUE_SIZE\s*=\s*8/);
  assert.match(worker, /decoder\.decodeQueueSize\s*>=\s*MAX_DECODE_QUEUE_SIZE/);
  assert.doesNotMatch(worker, /decodePending\s*>=/);
  assert.match(worker, /MAX_PACING_DELAY_MS\s*=\s*0/);
  assert.match(worker, /recovering:\s*true/);
  assert.match(worker, /setTimeout\(render/);
  assert.doesNotMatch(worker, /kind: "frame"/);
  assert.match(worker, /kind: "control"/);
});
