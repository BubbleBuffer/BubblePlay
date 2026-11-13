import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";
import { createLatencyGuard } from "../public/latency.js";
import { CODEC, prepareHybridWssFrame } from "../public/media.js";
import { detectAnnexBCodec, encodeFrameV4, parseFrameV4, V4ReceiveGate } from "../public/media-v4.js";
import { createCodecSession } from "../public/transport.js";
import { avcCodecFromSps } from "../public/wss-pipeline.js";

const source = readFileSync(new URL("../public/receiver.js", import.meta.url), "utf8");
const annexB = (nals, startBytes = 4) => Uint8Array.from(nals.flatMap(nal =>
  [...(startBytes === 4 ? [0, 0, 0, 1] : [0, 0, 1]), ...nal]));
const hevcKey = annexB([[0x46, 1, 0x50], [0x4e, 1, 0x80],
  [0x40, 1, 0x80], [0x42, 1, 0x80], [0x44, 1, 0x80], [0x26, 1, 0x80]]);
const avcKey = annexB([[0x09, 0xf0], [0x06, 0x80],
  [0x67, 0x64, 0x00, 0x2a, 0x80], [0x68, 0xeb, 0x80], [0x65, 0x88, 0x80]], 3);
const hevcDelta = annexB([[0x02, 1, 0x80]], 3);
const avcDelta = annexB([[0x41, 0x9a, 0x20]]);

test("Annex B classifier scans past AUD/SEI with either start code and HEVC IRAP type", () => {
  for (const start of [3, 4]) {
    assert.equal(detectAnnexBCodec(annexB([[9, 0xf0], [6, 0x80], [0x67, 0x64, 0, 42], [0x65, 0x88]], start)), "h264");
    assert.equal(detectAnnexBCodec(annexB([[9, 0xf0], [6, 0x80], [0x65, 0x88]], start)), "h264");
    assert.equal(detectAnnexBCodec(annexB([[0x68, 0xeb]], start)), "h264");
    for (const type of [19, 20, 21, 32, 33, 34]) {
      assert.equal(detectAnnexBCodec(annexB([[0x46, 1, 0x50], [0x4e, 1, 0x80], [type << 1, 1, 0x80]], start)), "hevc");
    }
  }
  const mixedStarts = Uint8Array.from([...annexB([[9, 0xf0]], 3), ...annexB([[0x67, 0x64, 0, 42]], 4)]);
  const padded = Uint8Array.from([0xff, ...mixedStarts, 0xff]);
  assert.equal(detectAnnexBCodec(padded.subarray(1, -1)), "h264", "honor view offsets");
  for (const data of [new Uint8Array(), Uint8Array.of(0, 0, 1), Uint8Array.of(0, 0, 0, 1),
    annexB([[9, 0xf0], [6, 0x80]]), annexB([[0xc2, 1]]), annexB([[0x42]])]) {
    assert.equal(detectAnnexBCodec(data), null);
  }
  assert.equal(detectAnnexBCodec(avcDelta), null, "a delta must not establish a keyframe codec");
  assert.equal(detectAnnexBCodec(avcDelta, { key: false }), "h264");
  assert.equal(detectAnnexBCodec(hevcDelta, { key: false }), "hevc");
  assert.equal(detectAnnexBCodec(annexB([[6, 1], [0x41, 0x9a]]), { key: false }), "h264");
});

function harness(usingWssMedia = true) {
  const decoders = [], decoded = [], failures = [], timers = new Map();
  let timerId = 0;
  class Decoder {
    constructor(callbacks) { this.callbacks = callbacks; this.state = "unconfigured"; this.decodeQueueSize = 0; this.chunks = []; decoders.push(this); }
    configure(config) { this.config = config; this.state = "configured"; }
    close() { this.state = "closed"; }
    decode(chunk) { this.chunks.push(chunk); this.decodeQueueSize++; }
    output() {
      const chunk = this.chunks.at(-1);
      this.decodeQueueSize = Math.max(0, this.decodeQueueSize - 1);
      this.callbacks.output({ timestamp: chunk.timestamp, close() {} });
    }
  }
  const context = vm.createContext({
    Uint8Array, WeakMap, Map, VideoDecoder: Decoder, V4_DECODE_BACKLOG_LIMIT_FRAMES: 30,
    EncodedVideoChunk: class { constructor(chunk) { Object.assign(this, chunk); } },
    performance: { now: () => 100 }, usingWssMedia, decoder: null,
    detectAnnexBCodec, prepareHybridWssFrame, CODEC, avcCodecFromSps, wssWorker: null, codecSession: createCodecSession({ transport: "wss" }),
    latencyGuard: createLatencyGuard({ onReset: () => assert.fail("codec switch must not reset the admitted generation") }),
    rtcStartupFallback: null, firstFrameHandshake: { requireAdmission: false },
    wssDecoderEpoch: 0, wssKeyframeTimer: null, wssNoOutputFailures: 0,
    wssRecoveryState: "healthy", wssRecoveryPending: false,
    wssDecodeSequenceByTimestamp: new Map(), wssFrameMeta: new WeakMap(), wssPresentedFrameTraces: new Map(),
    wssV4: { decodedSeq: 0, gate: new V4ReceiveGate() }, wssTimestampFallbackLogged: false,
    debugFirstDecoderOutput: false, debugFirstDecoderConfigured: false,
    hudState: { dropped: 0 }, activeWssHybridMode: () => false, activeWssHybridCache: () => ({}),
    activeWssHevcCodec: () => "hev1.1.6.L120.B0", clearWssHybridCaches() {},
    resetRenderPacing() {}, viewerLog() {}, debugTrace() {}, streamModeValue: () => "desktop",
    onDecodedFrame: frame => decoded.push(frame.timestamp),
    noteWssFailure: reason => failures.push(reason), beginWssRecovery: reason => failures.push(reason),
    requestKeyframe: reason => failures.push(reason), dropAndResync: reason => failures.push(reason),
    setTimeout: (fn, delay) => { const id = ++timerId; timers.set(id, { fn, delay }); return id; },
    clearTimeout: id => timers.delete(id),
  });
  const extract = (start, end) => source.slice(source.indexOf(start), source.indexOf(end, source.indexOf(start)));
  vm.runInContext([
    extract("function clearWssKeyframeWatchdog(", "function resyncWssDecoder("),
    extract("function decodeWssFrame(", "async function startPeer("),
    extract("function followDecoderCodec(", "function drawFrame("),
  ].join("\n"), context);
  return { context, decoders, decoded, failures, timers,
    feed(data, key, codec = CODEC.HEVC, seq = 0) {
      if (usingWssMedia) {
        const frame = parseFrameV4(encodeFrameV4({ data, key, codec, generation: 1, seq, captureTsUs: BigInt(seq + 1) }));
        assert.equal(context.wssV4.gate.accept(frame).decode, true);
        context.frame = { ...frame, sequence: frame.seq, timestampUs: Number(frame.captureTsUs) };
        vm.runInContext("decodeWssFrame(frame)", context);
      } else {
        context.chunk = { data, type: key ? "key" : "delta", timestamp: seq + 1 };
        vm.runInContext("feedDecoder(chunk, latencyGuard.generation)", context);
      }
    },
  };
}

test("WSS switches HEVC to SPS-derived AVC on that keyframe, despite a stale HEVC header", () => {
  const h = harness();
  h.feed(hevcKey, true, CODEC.HEVC, 0); h.decoders[0].output();
  h.feed(hevcDelta, false, CODEC.HEVC, 1); h.decoders[0].output();
  h.feed(avcDelta, false, CODEC.H264, 2);
  assert.equal(h.decoders[0].chunks.length, 2, "other-codec delta before keyframe stays dropped");
  h.context.wssRecoveryState = "awaiting-keyframe-output";
  h.feed(avcKey, true, CODEC.HEVC, 3);
  assert.equal(h.decoders.length, 2, "reconfigure synchronously without advancing a timer");
  assert.equal(h.decoders[0].state, "closed");
  assert.equal(h.decoders[0].config.codec, "hev1.1.6.L120.B0");
  assert.equal(h.decoders[1].config.codec, "avc1.64002A");
  assert.equal(h.context.codecSession.codec, "h264");
  assert.equal(h.decoders[1].chunks[0].type, "key");
  assert.deepEqual([...h.decoders[1].chunks[0].data], [...avcKey]);
  h.decoders[1].output();
  assert.equal(h.context.wssRecoveryState, "healthy");
  h.feed(avcDelta, false, CODEC.HEVC, 4); h.decoders[1].output();
  h.feed(avcDelta, false, CODEC.H264, 5); h.decoders[1].output();
  assert.equal(h.decoders[1].chunks.length, 3);
  assert.deepEqual(h.decoded, [1, 2, 4, 5, 6]);
  assert.deepEqual(h.failures, []);
  assert.equal(h.timers.size, 0);
});

test("codec replacement clears queued decode accounting and ignores old decoder callbacks", () => {
  const h = harness();
  h.feed(hevcKey, true, CODEC.HEVC, 0);
  const old = h.decoders[0];
  assert.equal(h.context.latencyGuard.decodeFrames, 1);
  h.feed(avcKey, true, CODEC.H264, 1);
  assert.equal(h.context.latencyGuard.decodeFrames, 1, "only the new keyframe remains queued");
  old.output(); old.callbacks.error(new Error("obsolete decoder"));
  assert.deepEqual(h.decoded, []);
  assert.deepEqual(h.failures, []);
  assert.equal(h.context.latencyGuard.decodeFrames, 1);
  assert.equal(h.timers.size, 1, "obsolete output cannot clear the new watchdog");
  h.decoders[1].output();
  h.feed(hevcKey, true, CODEC.H264, 2);
  assert.equal(h.decoders[2].config.codec, "hev1.1.6.L120.B0", "also follow an HEVC keyframe back");
  h.decoders[2].output();
  assert.equal(h.context.latencyGuard.decodeFrames, 0);
});

test("shared insertable-stream WebCodecs decoder follows keyframes with no codec metadata", () => {
  const h = harness(false);
  h.feed(hevcKey, true); h.decoders[0].output();
  h.feed(hevcDelta, false); h.decoders[0].output();
  h.feed(avcDelta, false);
  assert.equal(h.decoders.length, 1);
  h.feed(avcKey, true, undefined, 1); h.decoders[1].output();
  h.feed(avcDelta, false, undefined, 2); h.decoders[1].output();
  assert.equal(h.decoders[1].config.codec, "avc1.64002A");
  assert.equal(h.decoders[1].chunks.length, 2);
  assert.deepEqual(h.failures, []);
});

test("a first H264 keyframe overrides HEVC selection and unclassified headers cannot switch codec", () => {
  const h = harness();
  h.feed(annexB([[9, 0xf0], [6, 0x80]]), true, CODEC.H264, 0);
  assert.equal(h.decoders.length, 0, "a mismatched header alone must not reconfigure");
  h.feed(avcKey, true, CODEC.HEVC, 1); h.decoders[0].output();
  h.feed(avcKey, true, CODEC.HEVC, 2); h.decoders[0].output();
  assert.equal(h.decoders.length, 1, "same-codec keyframe reuses the decoder");
  assert.equal(h.decoders[0].config.codec, "avc1.64002A");
  assert.deepEqual(h.failures, []);
});

test("HEVC reconfiguration retains the existing Main10 codec string", () => {
  const h = harness();
  h.feed(avcKey, true, CODEC.HEVC, 0); h.decoders[0].output();
  h.context.activeWssHevcCodec = () => "hev1.2.4.L120.B0";
  h.feed(hevcKey, true, CODEC.H264, 1);
  assert.equal(h.decoders[1].config.codec, "hev1.2.4.L120.B0");
  h.decoders[1].output();
  assert.deepEqual(h.failures, []);
});

test("a raw H264 keyframe can leave an active HEVC hybrid session", () => {
  const h = harness();
  h.feed(hevcKey, true, CODEC.HEVC, 0); h.decoders[0].output();
  h.context.activeWssHybridMode = () => h.context.codecSession.codec === "hevc";
  h.feed(avcKey, true, CODEC.HEVC, 1); h.decoders[1].output();
  h.feed(hevcDelta, false, CODEC.H264, 2);
  assert.equal(h.decoders[1].chunks.length, 1, "opposite-codec deltas remain dropped after the switch");
  h.feed(avcDelta, false, CODEC.HEVC, 3); h.decoders[1].output();
  assert.equal(h.decoders[1].chunks.length, 2);
  assert.deepEqual(h.failures, []);
});
