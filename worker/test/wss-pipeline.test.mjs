import test from "node:test";
import assert from "node:assert/strict";
import { avcCodecFromSps, createWssPipeline } from "../public/wss-pipeline.js";

const HEVC_KEY = Uint8Array.of(0, 0, 0, 1, 0x40, 0x01, 0x0c, 0x01, 0xff);
const HEVC_DELTA = Uint8Array.of(0, 0, 0, 1, 0x02, 0x01, 0xaa, 0xbb, 0xcc);
const H264_KEY = Uint8Array.of(0, 0, 0, 1, 0x67, 0x64, 0x00, 0x2a, 0xac, 0, 0, 0, 1, 0x65, 0x88);

function fakeCanvas() {
  const draws = [];
  const context = { drawImage: (...args) => draws.push(args) };
  return { width: 0, height: 0, draws, getContext: () => context };
}

function harness({ queue = 0, sharp = false } = {}) {
  const posts = [];
  const decoders = [];
  const chunks = [];
  let clock = 1000;
  const pipeline = createWssPipeline({
    post: (message) => posts.push(message),
    now: () => clock,
    createDecoder: (init) => {
      const decoder = {
        init, state: "unconfigured", decodeQueueSize: queue, closed: false, decoded: [],
        configure(config) { this.config = config; this.state = "configured"; },
        decode(chunk) { this.decoded.push(chunk); chunks.push(chunk); },
        close() { this.closed = true; this.state = "closed"; },
      };
      decoders.push(decoder);
      return decoder;
    },
    createChunk: (init) => init,
  });
  const canvas = fakeCanvas();
  const sharpCanvas = fakeCanvas();
  pipeline.attach(sharp ? { main: canvas, sharp: sharpCanvas } : { main: canvas });
  const frame = (overrides = {}) => ({
    type: "frame", epoch: 0, codec: 1, generation: 1, sequence: 1, key: true, data: HEVC_KEY,
    timestampUs: 111, view: { enabled: false }, hevcCodec: "hev1.1.6.L120.B0",
    frameTrace: { sequence: 1, receivedAtMs: 5 }, ...overrides,
  });
  const output = (decoder, timestamp, w = 1920, h = 1080) => {
    const out = { timestamp, displayWidth: w, displayHeight: h, closed: false, close() { this.closed = true; } };
    decoder.init.output(out);
    return out;
  };
  return { pipeline, posts, decoders, chunks, canvas, sharpCanvas, frame, output, tick: (ms) => { clock += ms; } };
}

const types = (posts) => posts.map((message) => message.type);

test("a keyframe configures the decoder with the latency settings and decodes", () => {
  const h = harness();
  h.pipeline.handleFrame(h.frame());
  assert.equal(h.decoders.length, 1);
  assert.deepEqual(h.decoders[0].config, { codec: "hev1.1.6.L120.B0", optimizeForLatency: true, hardwareAcceleration: "prefer-hardware" });
  assert.equal(h.chunks[0].type, "key");
  assert.equal(h.chunks[0].timestamp, 111);
  assert.deepEqual(types(h.posts).filter((t) => t === "key-accepted" || t === "key-fed"), ["key-accepted", "key-fed"]);
});

test("deltas before a keyframe are dropped and recovery is requested", () => {
  const h = harness();
  h.pipeline.handleFrame(h.frame({ key: false, data: HEVC_DELTA, sequence: 2, timestampUs: 222 }));
  assert.equal(h.chunks.length, 0);
  assert.ok(h.posts.some((m) => m.type === "drop" && m.recover === "delta-before-keyframe"));
});

test("a decoded frame is drawn immediately to the canvas and reported with its sequence", () => {
  const h = harness();
  h.pipeline.handleFrame(h.frame());
  const frame = h.output(h.decoders[0], 111);
  assert.equal(h.canvas.width, 1920);
  assert.equal(h.canvas.height, 1080);
  assert.equal(h.canvas.draws.length, 1);
  assert.equal(frame.closed, true, "the VideoFrame is always closed");
  const out = h.posts.find((m) => m.type === "out");
  assert.equal(out.presented, true);
  assert.equal(out.resized, true);
  assert.equal(out.decodedSequence, 1);
  assert.equal(out.key, true);
  assert.equal(out.trace.presentedAtMs, 1000);
  assert.equal(out.trace.receivedAtMs, 5);
});

test("presentation is skipped while the decoder is behind but the frame is still reported as decoded", () => {
  const h = harness({ queue: 5 });
  h.pipeline.handleFrame(h.frame());
  h.output(h.decoders[0], 111); // msSinceLastPresent is Infinity: first frame still presents
  h.tick(10);
  h.pipeline.handleFrame(h.frame({ key: false, data: HEVC_DELTA, sequence: 2, timestampUs: 222 }));
  const frame = h.output(h.decoders[0], 222);
  const outs = h.posts.filter((m) => m.type === "out");
  assert.equal(outs.length, 2);
  assert.equal(outs[1].presented, false);
  assert.equal(outs[1].attempted, false);
  assert.equal(outs[1].decodedSequence, 2);
  assert.equal(frame.closed, true);
  assert.equal(h.canvas.draws.length, 1);
});

test("nothing is drawn while the desktop is unavailable or the generation has no geometry", () => {
  const h = harness();
  h.pipeline.handleFrame(h.frame({ desktopAvailable: false }));
  h.output(h.decoders[0], 111);
  assert.equal(h.canvas.draws.length, 0);
  h.tick(200);
  h.pipeline.handleFrame(h.frame({ key: false, data: HEVC_DELTA, sequence: 2, timestampUs: 222, view: null }));
  h.output(h.decoders[0], 222);
  assert.equal(h.canvas.draws.length, 0);
  assert.equal(h.posts.filter((m) => m.type === "out" && m.presented).length, 0);
});

test("frames from before a resync are ignored and the decoder is closed", () => {
  const h = harness();
  h.pipeline.handleFrame(h.frame());
  h.pipeline.resync({ epoch: 3, reason: "test" });
  assert.equal(h.decoders[0].closed, true);
  h.pipeline.handleFrame(h.frame({ epoch: 2 }));
  assert.equal(h.decoders.length, 1);
  h.pipeline.handleFrame(h.frame({ epoch: 3 }));
  assert.equal(h.decoders.length, 2);
  // Output from the closed decoder never reaches the canvas.
  const stale = h.output(h.decoders[0], 111);
  assert.equal(stale.closed, true);
  assert.equal(h.canvas.draws.length, 0);
});

test("a decoder error and a decode throw are reported as failures", () => {
  const h = harness();
  h.pipeline.handleFrame(h.frame());
  h.decoders[0].init.error(new Error("boom"));
  assert.deepEqual(h.posts.find((m) => m.type === "failure"), { type: "failure", reason: "decode-error", hud: "err:boom" });
  const t = harness();
  t.pipeline.handleFrame(t.frame());
  t.decoders[0].decode = () => { throw new Error("bad"); };
  t.pipeline.handleFrame(t.frame({ sequence: 2, key: true, timestampUs: 5 }));
  assert.ok(t.posts.some((m) => m.type === "failure" && m.reason === "decode-throw"));
});

test("a keyframe in another codec reconfigures the decoder and tells the main thread", () => {
  const h = harness();
  h.pipeline.handleFrame(h.frame());
  h.pipeline.handleFrame(h.frame({ codec: 2, data: H264_KEY, sequence: 2, timestampUs: 300 }));
  assert.equal(h.pipeline.codec, "h264");
  assert.deepEqual(h.posts.find((m) => m.type === "codec-follow"), { type: "codec-follow", previousCodec: "hevc", codec: "h264" });
  assert.equal(h.decoders[0].closed, true);
  assert.equal(h.decoders[1].config.codec, "avc1.64002A");
  // A delta in the old codec is ignored.
  const before = h.chunks.length;
  h.pipeline.handleFrame(h.frame({ key: false, data: HEVC_DELTA, sequence: 3, timestampUs: 400 }));
  assert.equal(h.chunks.length, before);
});

test("timestamps that drifted are associated by arrival order", () => {
  const h = harness();
  h.pipeline.handleFrame(h.frame());
  h.output(h.decoders[0], 112); // decoder normalized 111 -> 112
  assert.equal(h.posts.find((m) => m.type === "out").decodedSequence, 1);
  assert.ok(h.posts.some((m) => m.type === "log" && m.name === "wss-timestamp-association-fallback"));
});

test("avcCodecFromSps reads profile, constraints and level", () => {
  assert.equal(avcCodecFromSps(H264_KEY), "avc1.64002A");
  assert.equal(avcCodecFromSps(HEVC_DELTA), null);
});

test("a zoomed crop is drawn to the sharp canvas and leaves the base canvas alone", () => {
  const h = harness({ sharp: true });
  const crop = { enabled: true, x: 10, y: 20, width: 640, height: 360, desktopWidth: 1920, desktopHeight: 1080 };
  h.pipeline.handleFrame(h.frame({ view: { enabled: false } }));
  h.output(h.decoders[0], 111);
  assert.equal(h.canvas.draws.length, 1);
  h.tick(200);
  h.pipeline.handleFrame(h.frame({ key: false, data: HEVC_DELTA, sequence: 2, timestampUs: 222, view: crop }));
  h.output(h.decoders[0], 222, 640, 360);
  assert.equal(h.canvas.draws.length, 1, "base canvas untouched");
  assert.equal(h.sharpCanvas.draws.length, 1);
  assert.equal(h.sharpCanvas.width, 640);
  const out = h.posts.filter((m) => m.type === "out").at(-1);
  assert.equal(out.sharp, true);
  assert.deepEqual(out.view, crop);
  assert.equal(out.resized, undefined, "the base canvas was not resized");
});

test("a crop with no sharp canvas falls back to the base canvas", () => {
  const h = harness();
  h.pipeline.handleFrame(h.frame({ view: { enabled: true } }));
  h.output(h.decoders[0], 111);
  assert.equal(h.canvas.draws.length, 1);
});

function hybridWrap(accessUnit, { generation = 1, sequence = 1, cropRevision = 7 } = {}) {
  const bytes = new Uint8Array(32 + accessUnit.length);
  const view = new DataView(bytes.buffer);
  view.setUint32(0, 0x42504831);
  view.setUint8(4, 1);
  view.setUint32(8, generation);
  view.setUint32(12, sequence);
  view.setUint32(16, cropRevision);
  view.setUint32(20, accessUnit.length);
  bytes.set(accessUnit, 32);
  return bytes;
}

function fakeCache(name, calls) {
  return {
    ingest: (frame) => { calls.push([name, "ingest", frame.sequence]); return true; },
    draw: (ctx, metadata) => { calls.push([name, "draw", metadata.cropRevision]); return 2; },
    clear: () => calls.push([name, "clear"]),
    close: () => calls.push([name, "close"]),
    setDecoderCapability: (capability) => calls.push([name, "capability", capability.state]),
    snapshot: () => ({ name }),
  };
}

function hybridHarness() {
  const calls = [];
  const posts = [];
  const decoders = [];
  const pipeline = createWssPipeline({
    post: (m) => posts.push(m), now: () => 1000,
    createDecoder: (init) => { const d = { init, state: "unconfigured", decodeQueueSize: 0, configure() { this.state = "configured"; }, decode() {}, close() { this.state = "closed"; } }; decoders.push(d); return d; },
    createChunk: (init) => init,
    hybridTileCache: fakeCache("png", calls), hevcTileCache: fakeCache("hevc", calls),
  });
  pipeline.attach({ main: fakeCanvas(), sharp: fakeCanvas() });
  const send = (extra) => pipeline.handleFrame({ type: "frame", epoch: 0, codec: 1, generation: 1, sequence: 1, key: true,
    data: HEVC_KEY, timestampUs: 111, view: { enabled: false }, hybridMode: "png-static-roi", frameTrace: {}, ...extra });
  const output = (ts = 111) => decoders.at(-1).init.output({ timestamp: ts, displayWidth: 640, displayHeight: 360, close() {} });
  return { calls, posts, decoders, pipeline, send, output };
}

test("hybrid frames are unwrapped, their tiles ingested, and drawn over the base frame", () => {
  const h = hybridHarness();
  h.send({ data: hybridWrap(HEVC_KEY) });
  assert.deepEqual(h.calls.find((c) => c[1] === "ingest"), ["png", "ingest", 1]);
  h.output();
  assert.deepEqual(h.calls.find((c) => c[1] === "draw"), ["png", "draw", 7]);
  const out = h.posts.find((m) => m.type === "out");
  assert.equal(out.trace.hybridTilesDrawn, 2);
  assert.deepEqual(out.hybrid, { name: "png" });
});

test("the HEVC tile cache is used for hevc-quiescent-tiles", () => {
  const h = hybridHarness();
  h.send({ data: hybridWrap(HEVC_KEY), hybridMode: "hevc-quiescent-tiles" });
  assert.equal(h.calls.find((c) => c[1] === "ingest")[0], "hevc");
});

test("an unwrapped frame while a hybrid mode is active is rejected", () => {
  const h = hybridHarness();
  h.send({});
  assert.ok(h.posts.some((m) => m.type === "failure" && m.reason === "hybrid-frame-invalid"));
  assert.equal(h.decoders.length, 0);
});

test("with hybrid off frames pass through and the caches are never used", () => {
  const h = hybridHarness();
  h.send({ hybridMode: "off" });
  h.output();
  assert.equal(h.calls.filter((c) => c[1] === "ingest" || c[1] === "draw").length, 0);
});

test("resync, codec change and stop clear the caches; tile capability reaches the HEVC cache", () => {
  const h = hybridHarness();
  h.calls.length = 0;
  h.pipeline.resync({ epoch: 1 });
  assert.deepEqual(h.calls, [["png", "clear"], ["hevc", "clear"]]);
  h.pipeline.setTileCapability({ state: "supported" });
  assert.ok(h.calls.some((c) => c[0] === "hevc" && c[1] === "capability"));
  h.calls.length = 0;
  h.pipeline.stop();
  assert.deepEqual(h.calls, [["png", "clear"], ["hevc", "close"]]);
});
