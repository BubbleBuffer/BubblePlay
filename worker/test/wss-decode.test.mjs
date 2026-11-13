// The WSS decode worker wiring (public/wss-decode.js) on a fake worker client; every callback to
// the receiver is recorded in `calls`.
import test from "node:test";
import assert from "node:assert/strict";
import { createSessionState } from "../public/session-state.js";
import { createWssDecode } from "../public/wss-decode.js";

const EPOCH = 4;

function harness({ startResult = { ok: true }, transferred = true } = {}) {
  const calls = [];
  const log = name => (...args) => { calls.push([name, ...args]); };
  const posts = [];
  const client = {
    ready: true, transferred,
    start: async () => startResult,
    stop: () => calls.push(["client-stop"]),
    post: (message, transfer) => posts.push({ message, transfer }),
  };
  const state = createSessionState({});
  state.wssDecoderEpoch = EPOCH;
  state.wssRecoveryState = "awaiting-keyframe";
  const hudState = { dropped: 0 };
  const wssV4 = { presentSkips: 0, decodedSeq: undefined };
  const decode = createWssDecode({
    state, hudState, wssV4,
    codecSession: { codec: "hevc", followBitstreamCodec: codec => codec !== "hevc" },
    createClient: handlers => { client.handlers = handlers; return client; },
    frameTelemetry: { noteImmediateAttempt: log("attempt"), noteImmediatePresented: log("presented") },
    canvases: () => ({ main: "main", sharp: "sharp" }),
    log: log("log"), trace: log("trace"),
    setTimeout: () => 1, clearTimeout: log("clear-timeout"),
    defer: fn => { calls.push(["defer", fn.name]); },
    stopMedia: log("stop-media"), requestMediaStart: function requestMediaStart() {},
    ensureMainCanvas: log("main-canvas"), restoreCanvases: log("restore-canvases"),
    noteFailure: log("note-failure"), beginRecovery: log("begin-recovery"),
    armKeyframeWatchdog: log("arm-watchdog"), clearKeyframeWatchdog: log("clear-watchdog"), resetRenderPacing: log("reset-pacing"),
    hevcCodec: () => "hev1", hybridModeName: () => "off", cropForGeneration: generation => ({ generation }),
    layoutSharpCanvas: log("layout-sharp"), syncViewportAndFollow: log("sync-viewport"),
    noteDecodedDimensions: log("decoded-dims"), noteFrameReceived: log("frame-received"),
    firstFramePresented: log("first-frame"), notePresented: log("note-presented"),
  });
  return { decode, state, hudState, wssV4, calls, posts, client, names: () => calls.map(call => call[0]) };
}

const out = (extra = {}) => ({ type: "out", epoch: EPOCH, decodedSequence: 9, key: false, attempted: true, presented: true,
  width: 1920, height: 1080, sequence: 11, trace: { sequence: 11 }, renderedAt: 5, timestamp: 1, ...extra });

test("a started worker takes both canvases, then a resync for the epoch that is current by then", async () => {
  const h = harness();
  h.decode.begin();
  assert.equal(h.state.wssWorker, h.client);
  h.state.wssDecoderEpoch = EPOCH + 1;
  assert.equal(await h.state.wssWorkerInit, true);
  assert.deepEqual(h.posts[0].message, { type: "resync", epoch: EPOCH + 1, reason: "worker-start" });
  assert.equal(h.hudState.dec, "hevc wss (worker)");
});

test("a worker that cannot start gives the canvases back and decode stays on the main thread", async () => {
  const h = harness({ startResult: { ok: false, reason: "worker-timeout" } });
  h.decode.begin();
  assert.equal(await h.state.wssWorkerInit, false);
  assert.deepEqual(h.names().filter(name => name !== "log"), ["client-stop", "restore-canvases", "main-canvas"]);
  assert.equal(h.state.wssWorker, null);
});

test("a fatal worker error disables the worker path and restarts WSS media on the main thread", () => {
  const h = harness();
  h.decode.begin();
  h.client.handlers.onFatal("worker-error:boom");
  assert.equal(h.state.wssWorkerDisabled, true);
  assert.deepEqual(h.calls.filter(call => ["stop-media", "defer"].includes(call[0])), [["stop-media"], ["defer", "requestMediaStart"]]);
  assert.equal(h.hudState.dec, "worker:worker-error:boom");
});

test("frames go to the worker with the current epoch and the frame buffer moves", () => {
  const h = harness();
  h.state.wssWorker = h.client;
  const data = new Uint8Array([1, 2, 3]);
  h.decode.postFrame({ codec: 1, generation: 6, sequence: 2, key: true, timestampUs: 10, data, frameTrace: { sequence: 2 } });
  const { message, transfer } = h.posts[0];
  assert.deepEqual([message.type, message.epoch, message.generation, message.hevcCodec, message.view], ["frame", EPOCH, 6, "hev1", { generation: 6 }]);
  assert.equal(message.buffer, data.buffer);
  assert.deepEqual(transfer, [data.buffer]);
  assert.equal(h.hudState.mediaGeneration, 6);
});

test("decoder messages from before the last resync are ignored, current ones reach recovery", () => {
  const h = harness();
  h.state.wssWorker = h.client;
  h.decode.onMessage({ type: "failure", epoch: EPOCH - 1, reason: "stale" });
  h.decode.onMessage({ type: "failure", epoch: EPOCH, reason: "decode-error", hud: "decode failed" });
  h.decode.onMessage({ type: "drop", epoch: EPOCH, recover: "delta-before-keyframe" });
  h.decode.onMessage({ type: "key-fed", epoch: EPOCH });
  assert.deepEqual(h.calls, [["note-failure", "decode-error"], ["begin-recovery", "delta-before-keyframe"], ["arm-watchdog"]]);
  assert.equal(h.hudState.dec, "decode failed");
  assert.equal(h.hudState.dropped, 1);
});

test("recovery goes from awaiting a keyframe to healthy once that keyframe is decoded", () => {
  const h = harness();
  h.state.wssWorker = h.client;
  h.decode.onMessage({ type: "key-accepted", epoch: EPOCH, generation: 1, sequence: 2, codec: "hevc" });
  assert.equal(h.state.wssRecoveryState, "awaiting-keyframe-output");
  h.decode.onMessage(out({ key: false }));
  assert.equal(h.state.wssRecoveryState, "awaiting-keyframe-output", "a delta frame does not end the recovery");
  h.decode.onMessage(out({ key: true }));
  assert.equal(h.state.wssRecoveryState, "healthy");
  assert.equal(h.state.wssRecoveryPending, false);
  assert.equal(h.wssV4.decodedSeq, 9);
});

test("a frame the worker did not attempt to present counts as a skip, a presented one updates the page", () => {
  const h = harness();
  h.state.wssWorker = h.client;
  h.decode.onMessage(out({ presented: false, attempted: false }));
  assert.equal(h.wssV4.presentSkips, 1);
  assert.ok(!h.names().includes("first-frame"));
  h.calls.length = 0;
  h.decode.onMessage(out({ resized: true }));
  assert.deepEqual(h.names(), ["clear-watchdog", "frame-received", "attempt", "decoded-dims", "sync-viewport", "layout-sharp", "first-frame", "presented", "note-presented"]);
  assert.deepEqual(h.state.wssCanvasDims, { width: 1920, height: 1080 });
  assert.equal(h.state.wssPresentedSequence, 11);
  assert.equal(h.hudState.dimensions, "1920x1080");
});

test("a capture result resolves its waiter, and tearing the worker down resolves the rest with null", async () => {
  const h = harness();
  h.state.wssWorker = h.client;
  h.state.wssLatestFrameTrace = { sequence: 1 };
  const first = h.decode.capture({ sharp: false, diagnostics: () => ({ framesDrawn: 3 }) });
  const second = h.decode.capture({ sharp: true, diagnostics: () => ({}) });
  h.decode.onMessage({ type: "capture-result", id: h.posts[0].message.id, pngDataUrl: "png", glassToGlass: 12 });
  const result = await first;
  assert.equal(result.pngDataUrl, "png");
  assert.equal(result.diagnostics.framesDrawn, 3);
  h.decode.teardown();
  assert.equal(await second, null);
  assert.ok(h.names().includes("restore-canvases"));
});

test("a keyframe that changes codec resets render pacing and returns recovery to awaiting a keyframe", () => {
  const h = harness();
  h.state.wssWorker = h.client;
  h.state.wssRecoveryState = "awaiting-keyframe-output";
  h.decode.onMessage({ type: "codec-follow", epoch: EPOCH, codec: "h264", previousCodec: "hevc" });
  assert.equal(h.state.wssRecoveryState, "awaiting-keyframe");
  assert.ok(h.names().includes("reset-pacing"));
});
