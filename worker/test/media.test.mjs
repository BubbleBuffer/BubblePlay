import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import * as mediaModule from "../public/media.js";
import {
  CODEC,
  STREAM_LAB_ASSET_BUILD,
  STREAM_LAB_COMPATIBILITY,
  FRAME_HEADER_BYTES,
  MAX_FRAGMENT_BYTES,
  FrameReassembler,
  ParityFrameReassembler,
  OrderedFrameRelease,
  AdaptiveFrameDeadline,
  decodeMediaPacket,
  fragmentFrame,
  shardFrameV2,
  shardFrameV3,
  decodeMediaShardV3,
  classifyMediaShardV3,
  createSignalingRecycleDiagnostic,
  decodeHybridFrameV1,
  drawBaseThenHybrid,
  HybridTileCache,
  WssLaneLifecycleTelemetry,
  prepareHybridWssFrame,
  stripLabEdgeTrace,
  VariableParityFrameReassembler,
  V3_HEADER_BYTES,
  selectWssMediaTopology,
  selectStaleWssLane,
  shouldDeferWssMediaStart,
  shouldRecoverWssStripeReconnect,
  validateWssTopology,
} from "../public/media.js";
import {
  createCodecSession,
  parseAdaptive,
  parseQualityPreset,
  parseTransport,
  rtpCodecMimeTypes,
} from "../public/transport.js";

test("WSS decoded frames bypass RAF coalescing and present synchronously", () => {
  const events = ["before"];
  const frame = { id: 7 };
  const immediate = mediaModule.presentWssFrameImmediately?.(
    frame,
    true,
    123.5,
    (drawnFrame, due) => events.push(["draw", drawnFrame.id, due]),
  );
  events.push("after");

  assert.equal(immediate, true);
  assert.deepEqual(events, ["before", ["draw", 7, 123.5], "after"]);
  assert.equal(mediaModule.presentWssFrameImmediately?.(
    frame,
    false,
    200,
    () => events.push("unexpected"),
  ), false);
  assert.equal(events.includes("unexpected"), false);
});

test("presentation telemetry distinguishes synchronous WSS draws from RAF loss", () => {
  const telemetry = new mediaModule.FramePresentationTelemetry();
  const frame = { id: 11 };

  assert.equal(mediaModule.presentWssFrameImmediately(
    frame,
    true,
    125,
    () => telemetry.noteImmediatePresented(),
    telemetry,
  ), true);
  assert.equal(mediaModule.presentWssFrameImmediately(
    frame,
    false,
    150,
    () => telemetry.noteImmediatePresented(),
    telemetry,
  ), false);
  telemetry.noteRafEnqueued();
  telemetry.noteRafCoalesced(2);
  telemetry.noteRafLateDrop();

  assert.deepEqual(telemetry.snapshot(), {
    protocol: 1,
    immediateAttempts: 1,
    immediatePresented: 1,
    rafEnqueued: 1,
    rafCoalesced: 2,
    rafLateDrops: 1,
  });
  telemetry.reset();
  assert.deepEqual(telemetry.snapshot(), {
    protocol: 1,
    immediateAttempts: 0,
    immediatePresented: 0,
    rafEnqueued: 0,
    rafCoalesced: 0,
    rafLateDrops: 0,
  });
});

test("WSS lane lifecycle telemetry correlates a replacement socket with its Worker forward target", () => {
  const telemetry = new WssLaneLifecycleTelemetry({ eventLimit: 16 });
  const first = telemetry.construct(2, 1_000);
  telemetry.transportOpen(2, first, 1_010);
  telemetry.helloOk(2, first, 100_001, 1_020);
  telemetry.close(2, first, { code: 1006, reason: "lost", wasClean: false }, 1_030);
  telemetry.reconnectScheduled(2, first, "socket-close", 250, 1_031);
  telemetry.reconnectFired(2, first, "socket-close", 1_281);

  const replacement = telemetry.construct(2, 1_282);
  telemetry.transportOpen(2, replacement, 1_290);
  telemetry.helloOk(2, replacement, 100_002, 1_300);
  telemetry.mediaForward(2, replacement, 100_002, 1_305);
  telemetry.firstPacket(2, replacement, 49_176, 1_306);

  assert.equal(replacement, first + 1);
  const snapshot = telemetry.snapshot();
  assert.equal(snapshot[2].socketGeneration, replacement);
  assert.equal(snapshot[2].workerRegisteredAtMs, 100_002);
  assert.equal(snapshot[2].mediaForwardRegisteredAtMs, 100_002);
  assert.equal(snapshot[2].firstPacketBytes, 49_176);
  assert.equal(snapshot[2].state, "receiving");
  assert.deepEqual(snapshot[2].events.map(event => event.type), [
    "socket-construct", "transport-open", "hello-ok", "socket-close",
    "reconnect-scheduled", "reconnect-fired", "socket-construct", "transport-open",
    "hello-ok", "media-forward", "first-packet",
  ]);
});

test("WSS lane lifecycle telemetry ignores stale callbacks after a replacement is constructed", () => {
  const telemetry = new WssLaneLifecycleTelemetry();
  const first = telemetry.construct(7, 2_000);
  const replacement = telemetry.construct(7, 2_010);

  assert.equal(telemetry.close(7, first, { code: 1006, reason: "late-close" }, 2_020), false);
  telemetry.helloOk(7, replacement, 200_002, 2_030);
  assert.equal(telemetry.firstPacket(7, first, 100, 2_040), false);

  const lane = telemetry.snapshot()[7];
  assert.equal(lane.socketGeneration, replacement);
  assert.equal(lane.state, "registered");
  assert.equal(lane.workerRegisteredAtMs, 200_002);
  assert.equal(lane.firstPacketAtMs, null);
});

test("Stream Lab compatibility identifies the exact WSS trace and benchmark contract", () => {
  assert.equal(STREAM_LAB_ASSET_BUILD, "wss-v3-host20-edge12-bench2-hevc-tiles-bpip4-v15");
  assert.deepEqual(STREAM_LAB_COMPATIBILITY, {
    schemaVersion: 1,
    id: "wss-v3-host20-edge12-bench2-hevc-tiles-bpip4-v15",
    receiverBuild: "wss-v3-host20-edge12-bench2-hevc-tiles-bpip4-v15",
    mediaProtocol: 3,
    hostTrace: { version: 1, bytes: 20 },
    edgeTrace: { version: 1, bytes: 12 },
    benchStatusProtocol: 2,
    privilegedIpc: { version: 4 },
    roiModes: ["off", "damage", "text-edge", "feedback", "feedback-headroom"],
    hybridModes: ["off", "png-static-roi", "png-quiescent-detail", "hevc-quiescent-tiles"],
    hybridCacheModes: ["dda-gated", "suppress-reactivate"],
    requiredTelemetry: ["reconstructionFeedback:v1", "reconstructionFeedbackHeadroom:v1", "hybridActivity:png-quiescent-detail", "hevcQuiescentTiles:v1", "hybridDeferredReadback:v1",
      "ddaDamageAudit:v1", "hybridCacheSuppression:v1", "hybridCachePolicy:v1", "wssLaneLifecycle:v1",
      "signalingRecycle:v1", "networkRouteProbe:v2", "wssAckAttempt:v1",
      "wssImmediatePresentation:v1", "wssTailAwareDeadline:v1"],
  });
});

test("the candidate host media lanes advertise the same immutable compatibility generation", () => {
  const host = readFileSync(new URL("../../host/src/wss_media.rs", import.meta.url), "utf8");
  const advertised = host.match(/const STREAM_LAB_COMPATIBILITY_ID:\s*&str\s*=\s*"([^"]+)"/)?.[1];
  assert.equal(advertised, STREAM_LAB_COMPATIBILITY.id);
});

test("signaling recycle diagnostics retain only bounded WSS state and the initiating reason", () => {
  assert.deepEqual(createSignalingRecycleDiagnostic({
    reason: "network-wss-startup-timeout",
    signalingGeneration: 7,
    signalingRegistered: true,
    socketReadyState: 1,
    usingWssMedia: true,
    wssProtocol: 3,
    laneCount: 16,
    openLanes: 7,
    mediaGeneration: 12,
    reconstructed: 415,
    presented: 414,
    receivedPackets: 8_192,
    recoveryState: "healthy",
    renderedAgeMs: 3_150.4,
    online: true,
    ignoredSecret: "must-not-escape",
  }), {
    protocol: 1,
    reason: "network-wss-startup-timeout",
    signalingGeneration: 7,
    signalingRegistered: true,
    socketReadyState: 1,
    usingWssMedia: true,
    wssProtocol: 3,
    laneCount: 16,
    openLanes: 7,
    mediaGeneration: 12,
    reconstructed: 415,
    presented: 414,
    receivedPackets: 8_192,
    recoveryState: "healthy",
    renderedAgeMs: 3_150,
    online: true,
  });
  assert.equal(createSignalingRecycleDiagnostic({ reason: "secret/value?token=x" }).reason, "unknown");
});

test("hybrid frame parsing preserves the HEVC access unit and bounded tile operations", () => {
  const png = Uint8Array.of(137, 80, 78, 71, 13, 10, 26, 10, 1, 2, 3);
  const bytes = new Uint8Array(32 + 20 + png.byteLength + 5);
  const view = new DataView(bytes.buffer);
  view.setUint32(0, 0x42504831);
  view.setUint8(4, 1);
  view.setUint8(5, 2);
  view.setUint32(8, 7);
  view.setUint32(12, 41);
  view.setUint32(16, 3);
  view.setUint32(20, 5);
  view.setUint32(24, 20 + png.byteLength);
  let offset = 32;
  view.setUint8(offset, 2);
  view.setUint16(offset + 2, 9);
  offset += 4;
  view.setUint8(offset, 1);
  view.setUint16(offset + 2, 9);
  view.setUint16(offset + 4, 64);
  view.setUint16(offset + 6, 96);
  view.setUint16(offset + 8, 128);
  view.setUint16(offset + 10, 128);
  view.setUint32(offset + 12, png.byteLength);
  bytes.set(png, offset + 16);
  bytes.set(Uint8Array.of(0, 0, 0, 1, 38), 32 + 20 + png.byteLength);

  const decoded = decodeHybridFrameV1(bytes);
  assert.deepEqual([...decoded.accessUnit], [0, 0, 0, 1, 38]);
  assert.deepEqual(decoded.operations.map(operation => operation.type), ["invalidate", "upsert"]);
  assert.deepEqual(decoded.operations[1].rect, { x: 64, y: 96, width: 128, height: 128 });
});

test("hybrid frame parsing accepts only self-contained 128x128 HEVC tile IDRs", () => {
  const tileAccessUnit = Uint8Array.of(
    0, 0, 0, 1, 64, 1,
    0, 0, 0, 1, 66, 1,
    0, 0, 0, 1, 68, 1,
    0, 0, 0, 1, 38, 1,
  );
  const bytes = new Uint8Array(32 + 16 + tileAccessUnit.byteLength + 5);
  const view = new DataView(bytes.buffer);
  view.setUint32(0, 0x42504831);
  view.setUint8(4, 1);
  view.setUint8(5, 1);
  view.setUint32(8, 7);
  view.setUint32(12, 41);
  view.setUint32(16, 3);
  view.setUint32(20, 5);
  view.setUint32(24, 16 + tileAccessUnit.byteLength);
  view.setUint8(32, 7);
  view.setUint16(34, 9);
  view.setUint16(36, 64);
  view.setUint16(38, 96);
  view.setUint16(40, 128);
  view.setUint16(42, 128);
  view.setUint32(44, tileAccessUnit.byteLength);
  bytes.set(tileAccessUnit, 48);
  bytes.set(Uint8Array.of(0, 0, 0, 1, 38), 48 + tileAccessUnit.byteLength);

  const decoded = decodeHybridFrameV1(bytes);
  assert.deepEqual(decoded?.operations, [{
    type: "hevc-upsert",
    tileId: 9,
    rect: { x: 64, y: 96, width: 128, height: 128 },
    accessUnit: tileAccessUnit,
  }]);

  view.setUint16(40, 127);
  assert.equal(decodeHybridFrameV1(bytes), null);
  view.setUint16(40, 128);
  bytes[48 + 22] = 2;
  assert.equal(decodeHybridFrameV1(bytes), null);
});

test("hybrid frame parsing binds suppression verdicts to content and validation revisions", () => {
  const bytes = new Uint8Array(32 + 36 + 5);
  const view = new DataView(bytes.buffer);
  view.setUint32(0, 0x42504831);
  view.setUint8(4, 1);
  view.setUint8(5, 3);
  view.setUint32(8, 7);
  view.setUint32(12, 42);
  view.setUint32(16, 3);
  view.setUint32(20, 5);
  view.setUint32(24, 36);
  for (const [index, type] of [4, 5, 6].entries()) {
    const offset = 32 + index * 12;
    view.setUint8(offset, type);
    view.setUint16(offset + 2, 9 + index);
    view.setUint32(offset + 4, 39 + index);
    view.setUint32(offset + 8, 41);
  }
  bytes.set(Uint8Array.of(0, 0, 0, 1, 38), 68);

  const decoded = decodeHybridFrameV1(bytes);
  assert.deepEqual(decoded.operations, [
    { type: "suppress", tileId: 9, contentRevision: 39, validationSequence: 41 },
    { type: "reactivate", tileId: 10, contentRevision: 40, validationSequence: 41 },
    { type: "invalidate-validated", tileId: 11, contentRevision: 41, validationSequence: 41 },
  ]);
});

test("hybrid tile cache rejects stale generations and draws only after base video", async () => {
  const events = [];
  const cache = new HybridTileCache({
    decodeImage: async () => ({ close: () => events.push("close") }),
  });
  cache.ingest({
    generation: 7,
    sequence: 41,
    cropRevision: 3,
    operations: [{
      type: "upsert",
      tileId: 9,
      rect: { x: 64, y: 96, width: 128, height: 128 },
      png: Uint8Array.of(1, 2, 3),
    }],
  });
  await Promise.resolve();
  const baseFrame = {};
  const ctx = { drawImage: image => events.push(image === baseFrame ? "base" : "tile") };
  const drawn = drawBaseThenHybrid(ctx, baseFrame, cache,
    { generation: 7, sequence: 42, cropRevision: 3 }, 1920, 1080);
  assert.equal(drawn, 1);
  assert.deepEqual(events, ["base", "tile"]);
  assert.equal(cache.draw(ctx, { generation: 8, sequence: 42, cropRevision: 3 }), 0);
});

test("HEVC tile decoding is serial, sequence-bound, and activates after the base frame", () => {
  let handlers = null;
  const chunks = [];
  const decoder = {
    state: "unconfigured",
    decodeQueueSize: 0,
    configure(config) {
      this.state = "configured";
      this.config = config;
    },
    decode(chunk) { chunks.push(chunk); },
    reset() { this.state = "unconfigured"; },
    close() { this.state = "closed"; },
  };
  const cache = new mediaModule.HevcTileCache({
    createDecoder: callbacks => {
      handlers = callbacks;
      return decoder;
    },
    createChunk: init => init,
  });
  assert.equal(cache.ingest({
    generation: 7,
    sequence: 41,
    cropRevision: 3,
    operations: [{
      type: "hevc-upsert",
      tileId: 9,
      rect: { x: 64, y: 96, width: 128, height: 128 },
      accessUnit: Uint8Array.of(0, 0, 0, 1, 38, 1),
    }],
  }), true);
  assert.equal(chunks.length, 1);
  assert.equal(chunks[0].type, "key");

  const tileFrame = { timestamp: chunks[0].timestamp, close() {} };
  handlers.output(tileFrame);
  const events = [];
  const baseFrame = {};
  const ctx = { drawImage: image => events.push(image === baseFrame ? "base" : "tile") };
  assert.equal(drawBaseThenHybrid(ctx, baseFrame, cache,
    { generation: 7, sequence: 41, cropRevision: 3 }, 1920, 1080), 0);
  assert.equal(drawBaseThenHybrid(ctx, baseFrame, cache,
    { generation: 7, sequence: 42, cropRevision: 3 }, 1920, 1080), 1);
  assert.deepEqual(events, ["base", "base", "tile"]);
});

test("HEVC tile decoder capability is probed at the exact bounded configuration", async () => {
  const seen = [];
  const supported = await mediaModule.probeHevcTileDecoderSupport(async config => {
    seen.push(config);
    return { supported: true, config };
  });
  assert.deepEqual(seen, [{
    codec: "hev1.1.6.L120.B0",
    codedWidth: 128,
    codedHeight: 128,
    hardwareAcceleration: "prefer-hardware",
    optimizeForLatency: true,
  }]);
  assert.deepEqual(supported, { state: "supported", codec: "hev1.1.6.L120.B0" });

  const unsupported = await mediaModule.probeHevcTileDecoderSupport(
    async () => ({ supported: false }),
  );
  assert.deepEqual(unsupported, { state: "unsupported", codec: "hev1.1.6.L120.B0" });
});

test("an unsupported HEVC tile decoder bypasses enhancement without touching base video", () => {
  let decoderCreated = false;
  const cache = new mediaModule.HevcTileCache({
    createDecoder: () => {
      decoderCreated = true;
      throw new Error("must not create a decoder");
    },
    createChunk: init => init,
  });
  cache.setDecoderCapability({ state: "unsupported", codec: "hev1.1.6.L120.B0" });
  assert.equal(cache.ingest({
    generation: 7,
    sequence: 41,
    cropRevision: 3,
    operations: [{
      type: "hevc-upsert",
      tileId: 9,
      rect: { x: 64, y: 96, width: 128, height: 128 },
      accessUnit: Uint8Array.of(0, 0, 0, 1, 38, 1),
    }],
  }), true);
  assert.equal(decoderCreated, false);
  assert.equal(cache.snapshot().decoderCapability, "unsupported");
  assert.equal(cache.snapshot().decoderCapabilityBypasses, 1);
  assert.equal(cache.snapshot().decoderErrors, 0);
  assert.equal(cache.snapshot().activeTiles, 0);
});

test("HEVC tile decoder errors and stale validation operations fail back to base video", () => {
  let handlers = null;
  const chunks = [];
  const decoder = {
    state: "unconfigured", decodeQueueSize: 0,
    configure() { this.state = "configured"; },
    decode(chunk) { chunks.push(chunk); },
    reset() { this.state = "unconfigured"; },
    close() { this.state = "closed"; },
  };
  const cache = new mediaModule.HevcTileCache({
    createDecoder: callbacks => (handlers = callbacks, decoder),
    createChunk: init => init,
  });
  cache.ingest({
    generation: 7, sequence: 41, cropRevision: 3,
    operations: [{ type: "hevc-upsert", tileId: 9,
      rect: { x: 0, y: 0, width: 128, height: 128 },
      accessUnit: Uint8Array.of(0, 0, 0, 1, 38, 1) }],
  });
  handlers.output({ timestamp: chunks[0].timestamp, close() {} });
  cache.ingest({
    generation: 7, sequence: 42, cropRevision: 3,
    operations: [{ type: "suppress", tileId: 9,
      contentRevision: 999, validationSequence: 42 }],
  });
  assert.equal(cache.draw({ drawImage() {} },
    { generation: 7, sequence: 42, cropRevision: 3 }), 1);
  assert.equal(cache.snapshot().staleOperationRejects, 1);

  handlers.error(new Error("decode failed"));
  assert.equal(cache.draw({ drawImage() {} },
    { generation: 7, sequence: 43, cropRevision: 3 }), 0);
  assert.equal(cache.snapshot().activeTiles, 0);
  assert.equal(cache.snapshot().decoderErrors, 1);
});

test("hybrid tile invalidation prevents an intersected tile from a later draw", async () => {
  const cache = new HybridTileCache({ decodeImage: async () => ({ close() {} }) });
  cache.ingest({
    generation: 7,
    sequence: 10,
    cropRevision: 0,
    operations: [{
      type: "upsert",
      tileId: 4,
      rect: { x: 0, y: 0, width: 128, height: 128 },
      png: Uint8Array.of(1),
    }],
  });
  await Promise.resolve();
  cache.ingest({
    generation: 7,
    sequence: 11,
    cropRevision: 0,
    operations: [{ type: "invalidate", tileId: 4 }],
  });
  assert.equal(cache.draw({ drawImage() {} },
    { generation: 7, sequence: 12, cropRevision: 0 }), 0);
});

test("hybrid suppression retains the bitmap and exact deferred reactivation restores drawing", async () => {
  const events = [];
  const cache = new HybridTileCache({
    decodeImage: async () => ({ close: () => events.push("close") }),
  });
  cache.ingest({
    generation: 7,
    sequence: 10,
    cropRevision: 3,
    operations: [{
      type: "upsert",
      tileId: 9,
      rect: { x: 0, y: 0, width: 128, height: 128 },
      png: Uint8Array.of(1, 2, 3),
    }],
  });
  await Promise.resolve();
  cache.ingest({
    generation: 7,
    sequence: 11,
    cropRevision: 3,
    operations: [{ type: "suppress", tileId: 9, contentRevision: 10, validationSequence: 11 }],
  });
  assert.equal(cache.draw({ drawImage() { events.push("tile"); } },
    { generation: 7, sequence: 11, cropRevision: 3 }), 0);
  assert.deepEqual(events, []);
  assert.equal(cache.snapshot().suppressedTiles, 1);
  assert.equal(cache.snapshot().cachedBitmapBytes, 128 * 128 * 4);

  cache.ingest({
    generation: 7,
    sequence: 12,
    cropRevision: 3,
    operations: [{ type: "reactivate", tileId: 9, contentRevision: 10, validationSequence: 11 }],
  });
  assert.equal(cache.draw({ drawImage() { events.push("tile"); } },
    { generation: 7, sequence: 12, cropRevision: 3 }), 1);
  assert.deepEqual(events, ["tile"]);
});

test("hybrid reactivation rejects the wrong revision or validation sequence and remains suppressed", async () => {
  const cache = new HybridTileCache({ decodeImage: async () => ({ close() {} }) });
  cache.ingest({
    generation: 7,
    sequence: 10,
    cropRevision: 3,
    operations: [{
      type: "upsert",
      tileId: 9,
      rect: { x: 0, y: 0, width: 128, height: 128 },
      png: Uint8Array.of(1),
    }],
  });
  await Promise.resolve();
  cache.ingest({
    generation: 7,
    sequence: 11,
    cropRevision: 3,
    operations: [{ type: "suppress", tileId: 9, contentRevision: 10, validationSequence: 11 }],
  });
  cache.ingest({
    generation: 7,
    sequence: 12,
    cropRevision: 3,
    operations: [
      { type: "reactivate", tileId: 9, contentRevision: 9, validationSequence: 11 },
      { type: "reactivate", tileId: 9, contentRevision: 10, validationSequence: 10 },
    ],
  });
  assert.equal(cache.draw({ drawImage() {} },
    { generation: 7, sequence: 12, cropRevision: 3 }), 0);
  assert.equal(cache.snapshot().suppressedTiles, 1);
  assert.equal(cache.snapshot().staleOperationRejects, 2);
});

test("hybrid validated invalidation deletes only the exact suppressed cached revision", async () => {
  const events = [];
  const cache = new HybridTileCache({
    decodeImage: async () => ({ close: () => events.push("close") }),
  });
  cache.ingest({
    generation: 7,
    sequence: 10,
    cropRevision: 3,
    operations: [{
      type: "upsert",
      tileId: 9,
      rect: { x: 0, y: 0, width: 128, height: 128 },
      png: Uint8Array.of(1),
    }],
  });
  await Promise.resolve();
  cache.ingest({
    generation: 7,
    sequence: 11,
    cropRevision: 3,
    operations: [{ type: "suppress", tileId: 9, contentRevision: 10, validationSequence: 11 }],
  });
  cache.ingest({
    generation: 7,
    sequence: 12,
    cropRevision: 3,
    operations: [{
      type: "invalidate-validated",
      tileId: 9,
      contentRevision: 10,
      validationSequence: 11,
    }],
  });
  assert.deepEqual(events, ["close"]);
  assert.equal(cache.snapshot().activeTiles, 0);
  assert.equal(cache.snapshot().receivedPermanentInvalidations, 1);
});

test("hybrid stale-generation verdicts cannot alter the current crop cache", async () => {
  const events = [];
  const cache = new HybridTileCache({
    decodeImage: async () => ({ close: () => events.push("close") }),
  });
  cache.ingest({
    generation: 8,
    sequence: 20,
    cropRevision: 4,
    operations: [{
      type: "upsert",
      tileId: 9,
      rect: { x: 0, y: 0, width: 128, height: 128 },
      png: Uint8Array.of(1),
    }],
  });
  await Promise.resolve();
  assert.equal(cache.ingest({
    generation: 7,
    sequence: 21,
    cropRevision: 3,
    operations: [{ type: "reactivate", tileId: 9, contentRevision: 10, validationSequence: 11 }],
  }), false);
  assert.deepEqual(events, []);
  assert.equal(cache.snapshot().activeTiles, 1);
  assert.equal(cache.snapshot().cropRevision, 4);
});

test("hybrid base presentation never waits for a pending replacement bitmap", () => {
  const events = [];
  const cache = new HybridTileCache({ decodeImage: () => new Promise(() => {}) });
  cache.ingest({
    generation: 7,
    sequence: 10,
    cropRevision: 3,
    operations: [{
      type: "upsert",
      tileId: 9,
      rect: { x: 0, y: 0, width: 128, height: 128 },
      png: Uint8Array.of(1),
    }],
  });
  const base = {};
  const drawn = drawBaseThenHybrid({
    drawImage(image) { events.push(image === base ? "base" : "tile"); },
  }, base, cache, { generation: 7, sequence: 11, cropRevision: 3 }, 1920, 1080);
  assert.equal(drawn, 0);
  assert.deepEqual(events, ["base"]);
});

test("hybrid WSS preparation rejects mismatched envelopes and reports exact sidecar activity", () => {
  const png = Uint8Array.of(137, 80, 78, 71, 13, 10, 26, 10, 1);
  const bytes = new Uint8Array(32 + 16 + png.byteLength + 5);
  const view = new DataView(bytes.buffer);
  view.setUint32(0, 0x42504831);
  view.setUint8(4, 1);
  view.setUint8(5, 1);
  view.setUint32(8, 7);
  view.setUint32(12, 41);
  view.setUint32(16, 3);
  view.setUint32(20, 5);
  view.setUint32(24, 16 + png.byteLength);
  view.setUint8(32, 1);
  view.setUint16(34, 9);
  view.setUint16(36, 64);
  view.setUint16(38, 96);
  view.setUint16(40, 128);
  view.setUint16(42, 128);
  view.setUint32(44, png.byteLength);
  bytes.set(png, 48);
  bytes.set(Uint8Array.of(0, 0, 0, 1, 38), 48 + png.byteLength);
  const cache = new HybridTileCache({ decodeImage: async () => ({ close() {} }) });

  const beforeStatus = prepareHybridWssFrame({
    generation: 7, sequence: 41, data: bytes,
  }, { active: false, cache });
  assert.deepEqual([...beforeStatus.data], [0, 0, 0, 1, 38]);
  assert.equal(beforeStatus.cropRevision, 3);

  assert.equal(prepareHybridWssFrame({
    generation: 7, sequence: 42, data: bytes,
  }, { active: true, cache }), null);
  const prepared = prepareHybridWssFrame({
    generation: 7, sequence: 41, data: bytes,
  }, { active: true, cache });
  assert.deepEqual([...prepared.data], [0, 0, 0, 1, 38]);
  assert.equal(prepared.cropRevision, 3);
  assert.deepEqual(prepared.activity, {
    upserts: 1,
    hevcUpserts: 0,
    invalidations: 0,
    suppressions: 0,
    reactivations: 0,
    permanentInvalidations: 0,
    clears: 0,
    pngBytes: png.byteLength,
    hevcBytes: 0,
  });
});

test("hybrid cache diagnostics distinguish ready tiles from stale draw rejection", async () => {
  const cache = new HybridTileCache({ decodeImage: async () => ({ close() {} }) });
  cache.ingest({
    generation: 7,
    sequence: 10,
    cropRevision: 2,
    operations: [{
      type: "upsert",
      tileId: 4,
      rect: { x: 0, y: 0, width: 128, height: 128 },
      png: Uint8Array.of(1, 2, 3),
    }],
  });
  await Promise.resolve();
  assert.equal(cache.draw({ drawImage() {} },
    { generation: 8, sequence: 11, cropRevision: 2 }), 0);
  assert.deepEqual(cache.snapshot(), {
    generation: 7,
    cropRevision: 2,
    activeTiles: 1,
    drawableTiles: 1,
    suppressedTiles: 0,
    readyTiles: 1,
    pendingTiles: 0,
    cachedBitmapBytes: 128 * 128 * 4,
    receivedUpserts: 1,
    receivedInvalidations: 0,
    receivedClears: 0,
    receivedSuppressions: 0,
    receivedReactivations: 0,
    receivedPermanentInvalidations: 0,
    staleOperationRejects: 0,
    pngBytes: 3,
    composites: 0,
    staleDrawRejects: 1,
    staleComposites: 0,
    geometryTransitions: 1,
    baseOnlyTransitionFrames: 0,
    maxBaseOnlyTransitionFrames: 0,
    tileReadyDelayFrames: 0,
    maxTileReadyDelayFrames: 0,
  });
});

test("hybrid crop transitions present base immediately and activate detail one frame later", async () => {
  const cache = new HybridTileCache({ decodeImage: async () => ({ close() {} }) });
  const ctx = { drawImage() {} };
  cache.ingest({
    generation: 7,
    sequence: 10,
    cropRevision: 0,
    operations: [{
      type: "upsert",
      tileId: 1,
      rect: { x: 0, y: 0, width: 128, height: 128 },
      png: Uint8Array.of(1),
    }],
  });
  await Promise.resolve();
  assert.equal(cache.draw(ctx, { generation: 7, sequence: 10, cropRevision: 0 }), 0);
  assert.equal(cache.draw(ctx, { generation: 7, sequence: 11, cropRevision: 0 }), 1);

  cache.ingest({
    generation: 8,
    sequence: 20,
    cropRevision: 1,
    operations: [{
      type: "upsert",
      tileId: 1,
      rect: { x: 0, y: 0, width: 256, height: 256 },
      png: Uint8Array.of(2),
    }],
  });
  await Promise.resolve();
  assert.equal(cache.draw(ctx, { generation: 7, sequence: 21, cropRevision: 0 }), 0);
  assert.equal(cache.draw(ctx, { generation: 8, sequence: 20, cropRevision: 1 }), 0);
  assert.equal(cache.draw(ctx, { generation: 8, sequence: 21, cropRevision: 1 }), 1);
  assert.deepEqual(cache.snapshot(), {
    generation: 8,
    cropRevision: 1,
    activeTiles: 1,
    drawableTiles: 1,
    suppressedTiles: 0,
    readyTiles: 1,
    pendingTiles: 0,
    cachedBitmapBytes: 256 * 256 * 4,
    receivedUpserts: 2,
    receivedInvalidations: 0,
    receivedClears: 0,
    receivedSuppressions: 0,
    receivedReactivations: 0,
    receivedPermanentInvalidations: 0,
    staleOperationRejects: 0,
    pngBytes: 2,
    composites: 2,
    staleDrawRejects: 1,
    staleComposites: 0,
    geometryTransitions: 2,
    baseOnlyTransitionFrames: 2,
    maxBaseOnlyTransitionFrames: 1,
    tileReadyDelayFrames: 2,
    maxTileReadyDelayFrames: 1,
  });
});

test("lab edge timing trailers are stripped before v3 validation and preserve their epoch sample", () => {
  const topology = { laneCount: 5, dataShards: 4, parityShards: 1 };
  const [packet] = shardFrameV3({ codec: CODEC.HEVC, generation: 3, gop: 4, sequence: 5,
    timestamp90k: 6, key: true, topology, data: Uint8Array.of(10, 20, 30, 40, 50) });
  const source = new Uint8Array(packet);
  const tagged = new Uint8Array(source.byteLength + 24);
  tagged.set(source);
  const hostTrailer = new DataView(tagged.buffer, source.byteLength, 12);
  hostTrailer.setUint32(0, 0x42504854);
  hostTrailer.setFloat64(4, 1_787_940_000_100);
  const edgeTrailer = new DataView(tagged.buffer, source.byteLength + 12, 12);
  edgeTrailer.setUint32(0, 0x42504554);
  edgeTrailer.setFloat64(4, 1_787_940_000_123);

  const stripped = stripLabEdgeTrace(tagged.buffer);
  assert.equal(stripped.hostSentAtEpochMs, 1_787_940_000_100);
  assert.equal(stripped.edgeReceivedAtEpochMs, 1_787_940_000_123);
  assert.deepEqual([...new Uint8Array(stripped.packet)], [...source]);
  assert.equal(decodeMediaShardV3(stripped.packet)?.header.sequence, 5);
  assert.deepEqual(stripLabEdgeTrace(packet), { packet, edgeReceivedAtEpochMs: null });
});

test("extended lab host trailers bind exact frame ROI activity to the reconstructed sequence", () => {
  const topology = { laneCount: 5, dataShards: 4, parityShards: 1 };
  const [packet] = shardFrameV3({ codec: CODEC.HEVC, generation: 3, gop: 4, sequence: 17,
    timestamp90k: 6, key: false, topology, data: Uint8Array.of(10, 20, 30, 40, 50) });
  const source = new Uint8Array(packet);
  const tagged = new Uint8Array(source.byteLength + 20 + 12);
  tagged.set(source);
  const hostTrailer = new DataView(tagged.buffer, source.byteLength, 20);
  hostTrailer.setUint32(0, 0x42504854);
  hostTrailer.setFloat64(4, 1_787_940_000_100);
  hostTrailer.setUint8(12, 1);
  hostTrailer.setUint8(13, 1);
  hostTrailer.setUint16(14, 9);
  hostTrailer.setUint16(16, 2_040);
  const edgeTrailer = new DataView(tagged.buffer, source.byteLength + 20, 12);
  edgeTrailer.setUint32(0, 0x42504554);
  edgeTrailer.setFloat64(4, 1_787_940_000_123);

  const stripped = stripLabEdgeTrace(tagged.buffer);
  assert.equal(stripped.hostSentAtEpochMs, 1_787_940_000_100);
  assert.deepEqual(stripped.hostRoiTrace, {
    reason: "applied",
    changedCtbs: 9,
    totalCtbs: 2_040,
  });
  assert.deepEqual([...new Uint8Array(stripped.packet)], [...source]);
});

test("lab trace parsing classifies incompatible edge and host trailers", () => {
  const topology = { laneCount: 5, dataShards: 4, parityShards: 1 };
  const [packet] = shardFrameV3({ codec: CODEC.HEVC, generation: 3, gop: 4, sequence: 17,
    timestamp90k: 6, key: true, topology, data: Uint8Array.of(10, 20, 30, 40, 50) });
  const source = new Uint8Array(packet);
  const tagged = new Uint8Array(source.byteLength + 20 + 12);
  tagged.set(source);
  const hostTrailer = new DataView(tagged.buffer, source.byteLength, 20);
  hostTrailer.setUint32(0, 0x42504854);
  hostTrailer.setFloat64(4, 1_787_940_000_100);
  hostTrailer.setUint8(12, 2);
  const edgeTrailer = new DataView(tagged.buffer, source.byteLength + 20, 12);
  edgeTrailer.setUint32(0, 0x42504554);
  edgeTrailer.setFloat64(4, 1_787_940_000_123);

  assert.equal(stripLabEdgeTrace(tagged.buffer, {
    requireEdgeTrace: true,
    requireHostTrace: true,
  }).rejection, "host-trailer-version");

  edgeTrailer.setUint32(0, 0);
  assert.equal(stripLabEdgeTrace(tagged.buffer, {
    requireEdgeTrace: true,
    requireHostTrace: true,
  }).rejection, "edge-trailer-format");

  const edgeOnly = new Uint8Array(source.byteLength + 12);
  edgeOnly.set(source);
  const edgeOnlyTrailer = new DataView(edgeOnly.buffer, source.byteLength, 12);
  edgeOnlyTrailer.setUint32(0, 0x42504554);
  edgeOnlyTrailer.setFloat64(4, 1_787_940_000_123);
  assert.equal(stripLabEdgeTrace(edgeOnly.buffer, {
    requireEdgeTrace: true,
    requireHostTrace: true,
  }).rejection, "host-trailer-format");
});

test("v3 packet classification distinguishes wire, topology, generation, and lane failures", () => {
  const topology = { laneCount: 5, dataShards: 4, parityShards: 1 };
  const [packet] = shardFrameV3({ codec: CODEC.HEVC, generation: 3, gop: 4, sequence: 17,
    timestamp90k: 6, key: true, topology, data: Uint8Array.of(10, 20, 30, 40, 50) });
  assert.equal(classifyMediaShardV3(packet, { topology, lane: 0, generation: 3 }), null);

  const wrongVersion = packet.slice(0);
  new Uint8Array(wrongVersion)[0] = 2;
  assert.equal(classifyMediaShardV3(wrongVersion, { topology, lane: 0 }), "v3-header");
  assert.equal(classifyMediaShardV3(packet.slice(0, -1), { topology, lane: 0 }), "v3-length");
  assert.equal(classifyMediaShardV3(packet, {
    topology: { laneCount: 4, dataShards: 3, parityShards: 1 }, lane: 0,
  }), "topology");
  assert.equal(classifyMediaShardV3(packet, { topology, lane: 1 }), "logical-lane");
  assert.equal(classifyMediaShardV3(packet, { topology, lane: 5 }), "physical-lane");
  assert.equal(classifyMediaShardV3(packet, { topology, lane: 0, generation: 4 }), "generation");
});

test("v3 selects one open lane whose delivery stopped beyond the bounded lag and stall windows", () => {
  const openByLane = Array(16).fill(true);
  const highestSequenceByLane = Array(16).fill(687);
  const lastProgressAtMsByLane = Array(16).fill(9_900);
  highestSequenceByLane[10] = 644;
  lastProgressAtMsByLane[10] = 8_000;

  assert.equal(selectStaleWssLane({
    highestReconstructed: 687,
    highestSequenceByLane,
    lastProgressAtMsByLane,
    openByLane,
    nowMs: 10_000,
  }), 10);

  openByLane[10] = false;
  assert.equal(selectStaleWssLane({
    highestReconstructed: 687,
    highestSequenceByLane,
    lastProgressAtMsByLane,
    openByLane,
    nowMs: 10_000,
  }), null);
});

test("v3 stale-lane selection ignores transient lag, current progress, and wrap-ahead lanes", () => {
  assert.equal(selectStaleWssLane({
    highestReconstructed: 100,
    highestSequenceByLane: [91, 100, 101],
    lastProgressAtMsByLane: [8_900, 8_000, 8_000],
    openByLane: [true, true, true],
    nowMs: 10_000,
  }), null);
  assert.equal(selectStaleWssLane({
    highestReconstructed: 100,
    highestSequenceByLane: [80],
    lastProgressAtMsByLane: [9_500],
    openByLane: [true],
    nowMs: 10_000,
  }), null);
});

test("v3 reconnect handshake gives the physical lane a fresh stall grace window", () => {
  let nowMs = 1_000;
  const reassembler = new VariableParityFrameReassembler({
    topology: { laneCount: 4, dataShards: 3, parityShards: 1 },
    now: () => nowMs,
  });
  reassembler.highestReconstructed = 100;
  reassembler.highestSequence.fill(100);
  reassembler.highestSequence[2] = 50;
  nowMs = 4_000;

  assert.equal(reassembler.noteLaneConnected(2), true);
  assert.equal(selectStaleWssLane({
    highestReconstructed: reassembler.highestReconstructed,
    highestSequenceByLane: reassembler.highestSequence,
    lastProgressAtMsByLane: reassembler.lastProgressAtMs,
    openByLane: [true, true, true, true],
    nowMs,
  }), null);
});

test("transport accepts auto, webrtc, wss, and the wsl compatibility alias", () => {
  assert.equal(parseTransport(new URLSearchParams()), "auto");
  assert.equal(parseTransport(new URLSearchParams("transport=webrtc")), "webrtc");
  assert.equal(parseTransport(new URLSearchParams("transport=wss")), "wss");
  assert.equal(parseTransport(new URLSearchParams("wsl=1")), "wss");
  assert.equal(parseTransport(new URLSearchParams("transport=wsl")), "wss");
  assert.equal(parseTransport(new URLSearchParams("transport=unknown")), "auto");
});

test("WSS quality defaults high, remembers selection, and lets the query override it", () => {
  const memory = { getItem: () => "balanced" };
  assert.equal(parseQualityPreset(new URLSearchParams()), "high");
  assert.equal(parseQualityPreset(new URLSearchParams(), memory), "balanced");
  assert.equal(parseQualityPreset(new URLSearchParams("quality=saver"), memory), "saver");
  assert.equal(parseQualityPreset(new URLSearchParams("quality=auto"), memory), "auto");
  assert.equal(parseQualityPreset(new URLSearchParams("quality=invalid"), { getItem: () => "invalid" }), "high");
});

test("WSS adaptation follows auto unless explicitly overridden", () => {
  assert.equal(parseAdaptive(new URLSearchParams()), false);
  assert.equal(parseAdaptive(new URLSearchParams("adaptive=0")), false);
  assert.equal(parseAdaptive(new URLSearchParams("adaptive=1")), true);
  assert.equal(parseAdaptive(new URLSearchParams("quality=auto")), true);
  assert.equal(parseAdaptive(new URLSearchParams("quality=auto&adaptive=0")), false);
});

test("the page-local codec state starts HEVC and permits one H264 fallback", () => {
  const codecs = createCodecSession();
  assert.equal(codecs.codec, "hevc");
  assert.equal(codecs.fallbackToH264(), true);
  assert.equal(codecs.codec, "h264");
  assert.equal(codecs.fallbackToH264(), false);
});

test("HEVC RTP capability accepts Chromium H265 naming and its HEVC alias", () => {
  assert.deepEqual(rtpCodecMimeTypes("hevc"), ["video/h265", "video/hevc"]);
  assert.deepEqual(rtpCodecMimeTypes("h264"), ["video/h264"]);
});

test("media frames use bounded 48KiB fragments and round-trip all header fields", () => {
  const data = new Uint8Array(MAX_FRAGMENT_BYTES + 5).map((_, index) => index & 255);
  const packets = fragmentFrame({ codec: CODEC.HEVC, generation: 7, gop: 9, sequence: 11, timestamp90k: 13, key: true, data });
  assert.equal(packets.length, 2);
  assert.ok(packets.every((packet) => packet.byteLength <= FRAME_HEADER_BYTES + MAX_FRAGMENT_BYTES));
  assert.deepEqual(decodeMediaPacket(packets[0]).header, {
    version: 1, codec: CODEC.HEVC, generation: 7, gop: 9, sequence: 11,
    timestamp90k: 13, key: true, fragmentIndex: 0, fragmentCount: 2,
  });
  const reassembler = new FrameReassembler();
  assert.equal(reassembler.push(packets[1]), null);
  const frame = reassembler.push(packets[0]);
  assert.deepEqual(frame?.data, data);
});

test("reassembly rejects stale epochs and deltas until an IDR/key frame gates the stream", () => {
  const reassembler = new FrameReassembler();
  const frame = (generation, gop, sequence, key) => fragmentFrame({ codec: CODEC.HEVC, generation, gop, sequence, timestamp90k: sequence * 3_000, key, data: Uint8Array.of(sequence) })[0];
  assert.equal(reassembler.push(frame(4, 1, 1, false)), null);
  assert.equal(reassembler.push(frame(4, 1, 2, true))?.sequence, 2);
  assert.equal(reassembler.push(frame(4, 0, 3, false)), null);
  assert.equal(reassembler.push(frame(3, 2, 4, true)), null);
  assert.equal(reassembler.push(frame(5, 1, 5, false)), null);
  assert.equal(reassembler.push(frame(5, 1, 6, true))?.generation, 5);
});

test("reassembly keeps at most one incomplete frame per stripe", () => {
  const reassembler = new FrameReassembler();
  const large = new Uint8Array(MAX_FRAGMENT_BYTES + 1);
  const first = fragmentFrame({ codec: CODEC.HEVC, generation: 1, gop: 1, sequence: 1, timestamp90k: 1, key: true, data: large });
  const newer = fragmentFrame({ codec: CODEC.HEVC, generation: 1, gop: 1, sequence: 2, timestamp90k: 2, key: true, data: large });
  assert.equal(reassembler.push(first[0], 2), null);
  assert.equal(reassembler.push(newer[0], 2), null);
  assert.equal(reassembler.partial.size, 1);
  assert.equal(reassembler.push(first[1], 2), null);
});

test("v2 reconstructs the exact access unit with any one shard missing", () => {
  const data = new Uint8Array(250_003).map((_, index) => (index * 31) & 255);
  const packets = shardFrameV2({ codec: CODEC.HEVC, generation: 3, gop: 4, sequence: 5, timestamp90k: 6, key: true, data });
  assert.equal(packets.length, 4);
  for (let missing = 0; missing < 4; missing += 1) {
    const reassembler = new ParityFrameReassembler({ now: () => 0 });
    let frame = null;
    for (let stripe = 0; stripe < 4; stripe += 1) if (stripe !== missing) frame ||= reassembler.push(packets[stripe], stripe);
    assert.deepEqual(frame?.data, data);
    assert.equal(frame?.recovered, missing < 3);
  }
});

test("v2 final parity shard reconstructs before the same frame can expire", () => {
  let now = 0;
  const data = new Uint8Array(64_001).map((_, index) => (index * 23) & 255);
  const packets = shardFrameV2({
    codec: CODEC.HEVC,
    generation: 4,
    gop: 2,
    sequence: 90,
    timestamp90k: 270_000,
    key: true,
    data,
  });
  const reassembler = new ParityFrameReassembler({ now: () => now, ttlMs: 300 });
  assert.equal(reassembler.push(packets[1], 1), null);
  assert.equal(reassembler.push(packets[2], 2), null);
  now = 301;
  assert.deepEqual(reassembler.push(packets[3], 3)?.data, data);
  assert.deepEqual(reassembler.takeExpired(), []);
  assert.equal(reassembler.stats.expiredFrames, 0);
});

test("v2 reconstructed frames retain exact sequence and stage timing", () => {
  let now = 10;
  const packets = shardFrameV2({
    codec: CODEC.HEVC,
    generation: 8,
    gop: 2,
    sequence: 91,
    timestamp90k: 273_000,
    key: true,
    data: Uint8Array.of(1, 2, 3, 4, 5, 6),
  });
  const reassembler = new ParityFrameReassembler({ now: () => now });
  assert.equal(reassembler.push(packets[0], 0), null);
  now = 20;
  assert.equal(reassembler.push(packets[1], 1), null);
  now = 30;
  const frame = reassembler.push(packets[2], 2);
  assert.deepEqual(frame?.frameTrace, {
    sequence: 91,
    encodedBytes: 6,
    key: true,
    recovered: false,
    firstShardAtMs: 10,
    reconstructingShardAtMs: 30,
    reconstructedAtMs: 30,
  });
});

test("v3 topology is bounded and its 28-byte header is self-describing", () => {
  assert.equal(validateWssTopology({ laneCount: 16, dataShards: 15, parityShards: 1 }), true);
  assert.equal(validateWssTopology({ laneCount: 16, dataShards: 14, parityShards: 2 }), true);
  assert.equal(validateWssTopology({ laneCount: 1, dataShards: 1, parityShards: 0 }), true);
  assert.equal(validateWssTopology({ laneCount: 17, dataShards: 16, parityShards: 1 }), false);
  assert.equal(validateWssTopology({ laneCount: 5, dataShards: 3, parityShards: 1 }), false);
  const topology = { laneCount: 5, dataShards: 4, parityShards: 1 };
  const packets = shardFrameV3({ codec: CODEC.HEVC, generation: 3, gop: 4, sequence: 5,
    timestamp90k: 6, key: true, topology, data: Uint8Array.of(10, 20, 30, 40, 50) });
  assert.equal(packets.length, 5);
  assert.equal(new Uint8Array(packets[0]).byteLength >= V3_HEADER_BYTES, true);
  assert.deepEqual(decodeMediaShardV3(packets[4]).header, {
    version: 3, codec: CODEC.HEVC, key: true, shardIndex: 4,
    laneCount: 5, dataShards: 4, parityShards: 1,
    fecMode: 0, generation: 3, gop: 4, sequence: 5, timestamp90k: 6, originalLength: 5,
  });
});

test("v3 P/Q parity reconstructs the exact frame from any fourteen of sixteen lanes", () => {
  const topology = { laneCount: 16, dataShards: 14, parityShards: 2 };
  const data = new Uint8Array(4_103).map((_, index) => (index * 31) & 255);
  const packets = shardFrameV3({ codec: CODEC.HEVC, generation: 9, gop: 2, sequence: 77,
    timestamp90k: 9000, key: true, topology, data });
  assert.equal(packets.length, 16);
  assert.equal(new Uint8Array(packets[14])[7], 1);
  assert.notDeepEqual(new Uint8Array(packets[14]).subarray(V3_HEADER_BYTES),
    new Uint8Array(packets[15]).subarray(V3_HEADER_BYTES));
  for (let firstMissing = 0; firstMissing < topology.laneCount; firstMissing += 1) {
    for (let secondMissing = firstMissing + 1; secondMissing < topology.laneCount; secondMissing += 1) {
      const reassembler = new VariableParityFrameReassembler({ topology, now: () => 0 });
      let frame = null;
      for (let lane = 0; lane < topology.laneCount; lane += 1) {
        if (lane !== firstMissing && lane !== secondMissing) {
          frame ||= reassembler.push(packets[lane], lane);
        }
      }
      assert.deepEqual(frame?.data, data, `missing lanes ${firstMissing},${secondMissing}`);
    }
  }
});

test("v3 rejects a two-parity packet without the P/Q protocol marker", () => {
  const topology = { laneCount: 16, dataShards: 14, parityShards: 2 };
  const [packet] = shardFrameV3({ codec: CODEC.HEVC, generation: 9, gop: 2, sequence: 77,
    timestamp90k: 9000, key: true, topology, data: Uint8Array.of(1, 2, 3, 4) });
  const incompatible = packet.slice(0);
  new Uint8Array(incompatible)[7] = 0;
  assert.equal(decodeMediaShardV3(incompatible), null);
});

test("v3 P/Q parity matches the host GF(256) wire vector", () => {
  const topology = { laneCount: 6, dataShards: 4, parityShards: 2 };
  const packets = shardFrameV3({ codec: CODEC.H264, generation: 7, gop: 8, sequence: 9,
    timestamp90k: 0x0102_0304, key: true, topology, data: Uint8Array.of(1, 2, 3, 4, 5, 6, 7, 8) });
  assert.deepEqual([...new Uint8Array(packets[4]).subarray(V3_HEADER_BYTES)], [0, 8]);
  assert.deepEqual([...new Uint8Array(packets[5]).subarray(V3_HEADER_BYTES)], [43, 82]);
});

test("v3 frame traces distinguish validated WebSocket event time from handler time", () => {
  let now = 100;
  const topology = { laneCount: 4, dataShards: 3, parityShards: 1 };
  const packets = shardFrameV3({
    codec: CODEC.HEVC,
    generation: 9,
    gop: 2,
    sequence: 78,
    timestamp90k: 9_000,
    key: true,
    topology,
    data: Uint8Array.of(1, 2, 3, 4, 5, 6),
  });
  const reassembler = new VariableParityFrameReassembler({ topology, now: () => now });
  assert.equal(reassembler.push(packets[0], 0, 96), null);
  now = 110;
  assert.equal(reassembler.push(packets[1], 1, 108), null);
  now = 120;
  const frame = reassembler.push(packets[2], 2, 113);
  assert.equal(frame?.frameTrace.firstShardAtMs, 100);
  assert.equal(frame?.frameTrace.reconstructingShardAtMs, 120);
  assert.equal(frame?.frameTrace.firstShardEventAtMs, 96);
  assert.equal(frame?.frameTrace.firstShardHandlerAtMs, 100);
  assert.equal(frame?.frameTrace.firstShardQueueDelayMs, 4);
  assert.equal(frame?.frameTrace.completingShardEventAtMs, 113);
  assert.equal(frame?.frameTrace.completingShardHandlerAtMs, 120);
  assert.equal(frame?.frameTrace.completingShardQueueDelayMs, 7);
});

test("v3 frame traces fail closed when WebSocket event time is unavailable or uses another clock", () => {
  let now = 100;
  const topology = { laneCount: 4, dataShards: 3, parityShards: 1 };
  const packets = shardFrameV3({
    codec: CODEC.HEVC,
    generation: 9,
    gop: 2,
    sequence: 79,
    timestamp90k: 9_000,
    key: true,
    topology,
    data: Uint8Array.of(1, 2, 3, 4, 5, 6),
  });
  const reassembler = new VariableParityFrameReassembler({ topology, now: () => now });
  assert.equal(reassembler.push(packets[0], 0, 1_000_000_000_000), null);
  now = 110;
  assert.equal(reassembler.push(packets[1], 1, Number.NaN), null);
  now = 120;
  const frame = reassembler.push(packets[2], 2);
  assert.equal(frame?.frameTrace.firstShardEventAtMs, null);
  assert.equal(frame?.frameTrace.firstShardQueueDelayMs, null);
  assert.equal(frame?.frameTrace.completingShardEventAtMs, null);
  assert.equal(frame?.frameTrace.completingShardQueueDelayMs, null);
});

test("v3 records which lane missed the completion frontier and how late it arrived", () => {
  let now = 0;
  const topology = { laneCount: 6, dataShards: 5, parityShards: 1 };
  const packets = shardFrameV3({
    codec: CODEC.HEVC,
    generation: 3,
    gop: 4,
    sequence: 5,
    timestamp90k: 6,
    key: true,
    topology,
    data: new Uint8Array(101).map((_, index) => (index * 17) & 255),
  });
  const reassembler = new VariableParityFrameReassembler({ topology, now: () => now });
  for (const lane of [0, 1, 2, 3]) {
    assert.equal(reassembler.push(packets[lane], lane), null);
    now += 1;
  }
  const frame = reassembler.push(packets[5], 5);
  assert.equal(frame?.recovered, true);
  assert.deepEqual(reassembler.stats.completionMissingByLane, [0, 0, 0, 0, 1, 0]);

  now = 17;
  assert.equal(reassembler.push(packets[4], 4), null);
  assert.deepEqual(reassembler.stats.lateShardCountByLane, [0, 0, 0, 0, 1, 0]);
  assert.deepEqual(reassembler.stats.lateShardDelayMsTotalByLane, [0, 0, 0, 0, 13, 0]);
  assert.deepEqual(reassembler.stats.lateShardDelayMsMaxByLane, [0, 0, 0, 0, 13, 0]);
});

test("v3 reconstructs by logical shard while ACK and late-arrival telemetry stay physical-lane indexed", () => {
  let now = 0;
  const topology = { laneCount: 6, dataShards: 5, parityShards: 1 };
  const data = new Uint8Array(101).map((_, index) => (index * 29) & 255);
  const packets = shardFrameV3({
    codec: CODEC.HEVC,
    generation: 3,
    gop: 4,
    sequence: 5,
    timestamp90k: 6,
    key: true,
    topology,
    data,
  });
  // Physical lane 1 carries logical parity shard 5, while physical lane 5
  // carries logical data shard 1. All five data shards can therefore finish
  // without waiting for the slow physical parity lane.
  const logicalByPhysical = [0, 5, 2, 3, 4, 1];
  const reassembler = new VariableParityFrameReassembler({ topology, now: () => now });
  let frame = null;
  for (const physicalLane of [0, 2, 3, 4, 5]) {
    frame ||= reassembler.push(packets[logicalByPhysical[physicalLane]], physicalLane);
    now += 1;
  }
  assert.deepEqual(frame?.data, data);
  assert.equal(frame?.recovered, false);
  assert.deepEqual(reassembler.highestSequence, [5, 0, 5, 5, 5, 5]);
  assert.deepEqual(reassembler.stats.completionMissingByLane, [0, 1, 0, 0, 0, 0]);

  now = 19;
  assert.equal(reassembler.push(packets[logicalByPhysical[1]], 1), null);
  assert.deepEqual(reassembler.highestSequence, [5, 5, 5, 5, 5, 5]);
  assert.deepEqual(reassembler.stats.lateShardCountByLane, [0, 1, 0, 0, 0, 0]);
  assert.deepEqual(reassembler.stats.lateShardDelayMsTotalByLane, [0, 15, 0, 0, 0, 0]);
});

test("v3 decoding retains a zero-copy payload view over the WebSocket packet", () => {
  const topology = { laneCount: 4, dataShards: 3, parityShards: 1 };
  const [packet] = shardFrameV3({
    codec: CODEC.HEVC,
    generation: 3,
    gop: 4,
    sequence: 5,
    timestamp90k: 6,
    key: true,
    topology,
    data: Uint8Array.of(10, 20, 30, 40, 50),
  });
  const decoded = decodeMediaShardV3(packet);
  assert.equal(decoded.payload.buffer, packet);
  assert.equal(decoded.payload.byteOffset, V3_HEADER_BYTES);
});

test("production WSS selects validated 15+1 only for a fully capable v3 host and falls back safely", () => {
  const capable = { mediaProtocols: [2, 3], maxWssLanes: 16, wssFec: ["none", "xor-1"] };
  assert.deepEqual(selectWssMediaTopology({ hostCapabilities: capable }), {
    laneCount: 16, dataShards: 15, parityShards: 1,
  });
  assert.equal(selectWssMediaTopology({
    hostCapabilities: { ...capable, maxWssLanes: 15 },
  }), null);
  assert.equal(selectWssMediaTopology({
    hostCapabilities: { ...capable, mediaProtocols: [2] },
  }), null);
  assert.equal(selectWssMediaTopology({
    hostCapabilities: { ...capable, wssFec: ["none"] },
  }), null);
  assert.equal(selectWssMediaTopology({
    hostCapabilities: capable, v3Rejected: true,
  }), null);
  assert.equal(selectWssMediaTopology({
    hostCapabilities: capable, capabilitiesCurrent: false,
  }), null);

  const labTopology = { laneCount: 9, dataShards: 8, parityShards: 1 };
  assert.deepEqual(selectWssMediaTopology({
    requestedTopology: labTopology,
    hostCapabilities: { mediaProtocols: [2], maxWssLanes: 4, wssFec: ["xor-1"] },
  }), labTopology);
});

test("automatic WSS waits for current host capabilities while explicit lab topology starts immediately", () => {
  assert.equal(shouldDeferWssMediaStart({
    capabilitiesGeneration: 2,
    signalingGeneration: 3,
  }), true);
  assert.equal(shouldDeferWssMediaStart({
    capabilitiesGeneration: 3,
    signalingGeneration: 3,
  }), false);
  assert.equal(shouldDeferWssMediaStart({
    requestedTopology: { laneCount: 6, dataShards: 5, parityShards: 1 },
    capabilitiesGeneration: 2,
    signalingGeneration: 3,
  }), false);
});

test("a healthy parity lane reconnect preserves decoder state while non-parity reconnects recover", () => {
  assert.equal(shouldRecoverWssStripeReconnect({
    protocol: 3,
    topology: { laneCount: 16, dataShards: 15, parityShards: 1 },
  }), false);
  assert.equal(shouldRecoverWssStripeReconnect({
    protocol: 3,
    topology: { laneCount: 16, dataShards: 14, parityShards: 2 },
  }), false);
  assert.equal(shouldRecoverWssStripeReconnect({
    protocol: 3,
    topology: { laneCount: 4, dataShards: 4, parityShards: 0 },
  }), true);
  assert.equal(shouldRecoverWssStripeReconnect({
    protocol: 2,
    topology: { laneCount: 4, dataShards: 3, parityShards: 1 },
  }), true);
});

test("v3 reconstructs every bounded XOR topology with any one lane absent", () => {
  const data = new Uint8Array(65_539).map((_, index) => (index * 17) & 255);
  for (const laneCount of [2, 3, 4, 5, 9, 16]) {
    const topology = { laneCount, dataShards: laneCount - 1, parityShards: 1 };
    const packets = shardFrameV3({ codec: CODEC.HEVC, generation: 9, gop: 2, sequence: 77,
      timestamp90k: 9000, key: true, topology, data });
    for (let missing = 0; missing < laneCount; missing += 1) {
      const reassembler = new VariableParityFrameReassembler({ topology, now: () => 0 });
      let frame = null;
      for (let lane = 0; lane < laneCount; lane += 1) if (lane !== missing) frame ||= reassembler.push(packets[lane], lane);
      assert.deepEqual(frame?.data, data, `laneCount=${laneCount} missing=${missing}`);
      assert.equal(frame?.recovered, missing < topology.dataShards);
    }
  }
});

test("v3 final parity shard reconstructs before the same frame can expire", () => {
  let now = 0;
  const topology = { laneCount: 16, dataShards: 15, parityShards: 1 };
  const data = new Uint8Array(96_001).map((_, index) => (index * 29) & 255);
  const packets = shardFrameV3({
    codec: CODEC.HEVC,
    generation: 4,
    gop: 2,
    sequence: 91,
    timestamp90k: 273_000,
    key: true,
    topology,
    data,
  });
  const reassembler = new VariableParityFrameReassembler({ topology, now: () => now, ttlMs: 300 });
  for (let lane = 1; lane < topology.dataShards; lane += 1) {
    assert.equal(reassembler.push(packets[lane], lane), null);
  }
  now = 301;
  assert.deepEqual(reassembler.push(packets[15], 15)?.data, data);
  assert.deepEqual(reassembler.takeExpired(), []);
  assert.equal(reassembler.stats.expiredFrames, 0);
});

test("v3 retains faster-lane delta shards while a recovery keyframe awaits its completion lane", () => {
  const topology = { laneCount: 16, dataShards: 15, parityShards: 1 };
  const keyData = new Uint8Array(32_001).map((_, index) => (index * 17) & 255);
  const deltaData = new Uint8Array(24_001).map((_, index) => (index * 29) & 255);
  const keyPackets = shardFrameV3({
    codec: CODEC.HEVC,
    generation: 4,
    gop: 2,
    sequence: 90,
    timestamp90k: 270_000,
    key: true,
    topology,
    data: keyData,
  });
  const deltaPackets = shardFrameV3({
    codec: CODEC.HEVC,
    generation: 4,
    gop: 2,
    sequence: 91,
    timestamp90k: 273_000,
    key: false,
    topology,
    data: deltaData,
  });
  const reassembler = new VariableParityFrameReassembler({ topology, now: () => 0 });

  // Fourteen fast lanes advance into the dependent frame before lane 14
  // supplies the fifteenth shard needed to complete the recovery IDR.
  for (let lane = 0; lane < 14; lane += 1) {
    assert.equal(reassembler.push(keyPackets[lane], lane), null);
    assert.equal(reassembler.push(deltaPackets[lane], lane), null);
  }
  assert.deepEqual(reassembler.push(keyPackets[14], 14)?.data, keyData);

  // The next packet on the completion lane must combine with the fourteen
  // already delivered shards instead of leaving an unrecoverable sequence gap.
  assert.deepEqual(reassembler.push(deltaPackets[14], 14)?.data, deltaData);
  assert.equal(reassembler.stats.expiredFrames, 0);
});

test("v3 reconstructed frames retain first-shard and reconstruction timing", () => {
  let now = 10;
  const topology = { laneCount: 4, dataShards: 3, parityShards: 1 };
  const packets = shardFrameV3({
    codec: CODEC.HEVC,
    generation: 8,
    gop: 2,
    sequence: 91,
    timestamp90k: 273_000,
    key: true,
    topology,
    data: Uint8Array.of(1, 2, 3, 4, 5, 6),
  });
  const reassembler = new VariableParityFrameReassembler({ topology, now: () => now });
  assert.equal(reassembler.push(packets[0], 0, null, 1_787_940_000_123, 1_787_940_000_100), null);
  now = 20;
  assert.equal(reassembler.push(packets[1], 1), null);
  now = 30;
  const frame = reassembler.push(packets[2], 2);
  assert.deepEqual(frame?.frameTrace, {
    sequence: 91,
    encodedBytes: 6,
    key: true,
    recovered: false,
    firstShardAtMs: 10,
    firstShardEdgeAtEpochMs: 1_787_940_000_123,
    firstShardHostSentAtEpochMs: 1_787_940_000_100,
    firstShardEventAtMs: null,
    firstShardHandlerAtMs: 10,
    firstShardQueueDelayMs: null,
    reconstructingShardAtMs: 30,
    completingShardEventAtMs: null,
    completingShardHandlerAtMs: 30,
    completingShardQueueDelayMs: null,
    reconstructedAtMs: 30,
  });
});

test("v3 ignores a delayed returning-lane shard after a five-second parity-completed window", () => {
  let now = 0;
  const topology = { laneCount: 16, dataShards: 15, parityShards: 1 };
  const reassembler = new VariableParityFrameReassembler({ topology, now: () => now, ttlMs: 120 });
  let firstPackets = null;
  for (let sequence = 1; sequence <= 150; sequence += 1) {
    const packets = shardFrameV3({
      codec: CODEC.HEVC,
      generation: 4,
      gop: 2,
      sequence,
      timestamp90k: sequence * 3_000,
      key: sequence === 1,
      topology,
      data: Uint8Array.of(sequence & 255, (sequence * 3) & 255),
    });
    firstPackets ??= packets;
    let frame = null;
    for (let lane = 1; lane < topology.laneCount; lane += 1) {
      frame ||= reassembler.push(packets[lane], lane);
    }
    assert.equal(frame?.sequence, sequence);
  }
  assert.equal(reassembler.takeImmediateAck(), false,
    "ordinary reconstruction must retain batched ACK behavior");
  assert.equal(reassembler.push(firstPackets[0], 0), null);
  assert.equal(reassembler.highestSequence[0], 1,
    "a delayed completed-frame shard must still advance that lane's transport ACK");
  assert.equal(reassembler.takeImmediateAck(), true,
    "a delayed completed-frame shard must release its host recovery gate immediately");
  assert.equal(reassembler.takeImmediateAck(), false,
    "the immediate ACK signal must be consumed exactly once");
  now = 121;
  assert.deepEqual(reassembler.expire(), []);
  assert.equal(reassembler.pending.size, 0);
  assert.equal(reassembler.stats.expiredFrames, 0);
});

test("v3 no-parity mode requires every data lane and rejects topology mismatch", () => {
  const topology = { laneCount: 3, dataShards: 3, parityShards: 0 };
  const packets = shardFrameV3({ codec: CODEC.H264, generation: 1, gop: 1, sequence: 1,
    timestamp90k: 1, key: true, topology, data: Uint8Array.of(1, 2, 3, 4, 5) });
  const reassembler = new VariableParityFrameReassembler({ topology, now: () => 0 });
  assert.equal(reassembler.push(packets[0], 0), null);
  assert.equal(reassembler.push(packets[1], 1), null);
  assert.deepEqual(reassembler.push(packets[2], 2)?.data, Uint8Array.of(1, 2, 3, 4, 5));
  const wrong = new VariableParityFrameReassembler({ topology: { laneCount: 2, dataShards: 1, parityShards: 1 } });
  assert.equal(wrong.push(packets[0], 0), null);
});

test("v2 rejects crossed stripes and expires two-missing-shard frames at 120ms", () => {
  let now = 0;
  const reassembler = new ParityFrameReassembler({ now: () => now });
  const packets = shardFrameV2({ codec: CODEC.H264, generation: 1, gop: 1, sequence: 1, timestamp90k: 1, key: true, data: Uint8Array.of(1, 2, 3, 4) });
  assert.equal(reassembler.push(packets[0], 1), null);
  assert.equal(reassembler.push(packets[0], 0), null);
  assert.equal(reassembler.push(packets[1], 1), null);
  now = 121; reassembler.expire();
  assert.equal(reassembler.stats.expiredFrames, 1);
  assert.equal(reassembler.waitingForKey, true);
  assert.equal(reassembler.pending.size, 0);
});

test("late parity shard after three-shard reconstruction is ignored without creating expiry state", () => {
  let now=0; const reassembler=new ParityFrameReassembler({now:()=>now});
  const packets=shardFrameV2({codec:CODEC.HEVC,generation:1,gop:1,sequence:7,timestamp90k:7,key:true,data:Uint8Array.of(1,2,3,4,5)});
  let frame=null; for (let stripe=0;stripe<3;stripe++) frame ||= reassembler.push(packets[stripe],stripe);
  assert.equal(frame.sequence,7); assert.equal(reassembler.push(packets[3],3),null);
  now=200; reassembler.expire(); assert.equal(reassembler.stats.expiredFrames,0); assert.equal(reassembler.pending.size,0);
});

test("v2 ordered release never feeds deltas across a 120ms sequence gap", () => {
  let now=0; const release=new OrderedFrameRelease({now:()=>now});
  const frame=(sequence,key=false)=>({generation:1,gop:1,sequence,key});
  assert.deepEqual(release.push(frame(10,true)).map(x=>x.sequence),[10]);
  assert.deepEqual(release.push(frame(12)),[]);
  now=121; assert.equal(release.expire(),true); assert.equal(release.waitingForKey,true);
  assert.deepEqual(release.push(frame(13)),[]);
  assert.deepEqual(release.push(frame(14,true)).map(x=>x.sequence),[14]);
});

test("a newer IDR immediately supersedes an unresolved delta gap", () => {
  const release=new OrderedFrameRelease();
  const frame=(sequence,key=false)=>({generation:1,gop:key?2:1,sequence,key});
  assert.deepEqual(release.push(frame(20,true)).map(x=>x.sequence),[20]);
  assert.deepEqual(release.push(frame(22)),[]);
  assert.deepEqual(release.push(frame(30,true)).map(x=>x.sequence),[30]);
  assert.equal(release.waitingForKey,false);
});

test("explicit decoder resync drops partial frames but preserves lane ACK progress", () => {
  const reassembler=new ParityFrameReassembler();
  const packets=shardFrameV2({codec:CODEC.HEVC,generation:2,gop:1,sequence:9,timestamp90k:9,key:true,data:Uint8Array.of(1,2,3)});
  reassembler.push(packets[0],0);
  assert.equal(reassembler.pending.size,1);
  assert.equal(reassembler.highestSequence[0],9);
  reassembler.waitForKeyframe();
  assert.equal(reassembler.pending.size,0);
  assert.equal(reassembler.highestSequence[0],9);
  assert.equal(reassembler.waitingForKey,true);
});

test("mode-specific adaptive deadlines stay inside their latency budgets", () => {
  let now = 0;
  const work = new AdaptiveFrameDeadline({ mode: "work", now: () => now });
  assert.equal(work.deadlineMs, 300);
  work.observeCompletion(390);
  assert.equal(work.deadlineMs, 400);
  work.noteExpiry();
  assert.equal(work.deadlineMs, 400);

  const game = new AdaptiveFrameDeadline({ mode: "game", now: () => now });
  assert.equal(game.deadlineMs, 120);
  game.observeCompletion(145);
  assert.equal(game.deadlineMs, 150);
  game.noteExpiry();
  assert.equal(game.deadlineMs, 150);
  now = 5_001;
  game.tick();
  assert.ok(game.deadlineMs >= 80 && game.deadlineMs <= 150);
});

test("late parity-completed lane tails preserve bounded work deadline headroom", () => {
  let now = 0;
  const topology = { laneCount: 16, dataShards: 15, parityShards: 1 };
  const deadline = new AdaptiveFrameDeadline({ mode: "work", now: () => now });
  for (let step = 0; step < 12; step += 1) {
    now += 5_001;
    deadline.tick();
  }
  assert.equal(deadline.deadlineMs, 180);

  const packets = shardFrameV3({
    codec: CODEC.HEVC,
    generation: 4,
    gop: 2,
    sequence: 91,
    timestamp90k: 273_000,
    key: true,
    topology,
    data: new Uint8Array(32_001).map((_, index) => (index * 29) & 255),
  });
  const reassembler = new VariableParityFrameReassembler({ topology, now: () => now, ttlMs: 300 });
  let frame = null;
  for (let lane = 1; lane < topology.laneCount; lane += 1) {
    frame ||= reassembler.push(packets[lane], lane);
  }
  assert.equal(frame?.sequence, 91);

  now += 219;
  assert.equal(reassembler.push(packets[0], 0), null);
  assert.equal(typeof reassembler.takeLateShardAges, "function");
  const tailAges = reassembler.takeLateShardAges();
  assert.deepEqual(tailAges, [219]);
  for (const ageMs of tailAges) deadline.observeTail(ageMs);

  assert.equal(deadline.deadlineMs, 259);
  assert.equal(reassembler.stats.expiredFrames, 0);
});

test("late shard evidence updates both browser expiry boundaries before the packet handler returns", () => {
  let now = 0;
  const deadline = new AdaptiveFrameDeadline({ mode: "work", now: () => now });
  for (let step = 0; step < 12; step += 1) {
    now += 5_001;
    deadline.tick();
  }
  assert.equal(deadline.deadlineMs, 180);

  const pendingAges = [218.2, 219];
  const reassembler = {
    ttlMs: deadline.deadlineMs,
    takeLateShardAges: () => pendingAges.splice(0),
    setTtlMs(value) { this.ttlMs = value; },
  };
  const ordered = {
    ttlMs: deadline.deadlineMs,
    setTtlMs(value) { this.ttlMs = value; },
  };

  assert.equal(typeof mediaModule.applyLateShardDeadline, "function");
  const update = mediaModule.applyLateShardDeadline({ reassembler, ordered, deadline });

  assert.deepEqual(update, {
    observations: 2,
    maxAgeMs: 219,
    previousDeadlineMs: 180,
    deadlineMs: 259,
    raisedByMs: 79,
  });
  assert.equal(reassembler.ttlMs, 259);
  assert.equal(ordered.ttlMs, 259);
});

test("a newer partial expiry waits until it becomes the blocking dependency", () => {
  let now = 0;
  const reassembler = new ParityFrameReassembler({ now: () => now, ttlMs: 300 });
  const release = new OrderedFrameRelease({ now: () => now, ttlMs: 300 });
  const frame = (sequence, key = false) => shardFrameV2({
    codec: CODEC.HEVC, generation: 1, gop: 1, sequence,
    timestamp90k: sequence * 3_000, key, data: Uint8Array.of(sequence),
  });
  let key = null;
  for (let stripe = 0; stripe < 3; stripe += 1) key ||= reassembler.push(frame(10, true)[stripe], stripe);
  assert.deepEqual(release.push(key).map(value => value.sequence), [10]);
  const later = frame(12);
  reassembler.push(later[0], 0);
  reassembler.push(later[1], 1);
  now = 301;
  const expired = reassembler.expire();
  assert.deepEqual(expired.map(value => value.sequence), [12]);
  assert.equal(reassembler.waitingForKey, false);
  for (const missing of expired) release.markLost(missing.sequence);
  assert.equal(release.dependencyLosses, 0);
  assert.deepEqual(release.push({ generation: 1, gop: 1, sequence: 11, key: false }).map(value => value.sequence), [11]);
  assert.equal(release.dependencyLosses, 1);
  assert.equal(release.waitingForKey, true);
});
