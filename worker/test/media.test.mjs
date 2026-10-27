import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import * as mediaModule from "../public/media.js";
import {
  CODEC,
  createSignalingRecycleDiagnostic,
  decodeHybridFrameV1,
  drawBaseThenHybrid,
  HybridTileCache,
  prepareHybridWssFrame,
  shouldFallbackToWssOnMissingRtp,
} from "../public/media.js";
import {
  createCodecSession,
  parseQualityPreset,
  parseTransport,
  rememberedRtcFailure,
  rememberRtcFailure,
  forgetRtcFailure,
  RTC_FAILURE_STORAGE_KEY,
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

test("signaling recycle diagnostics retain only bounded WSS state and the initiating reason", () => {
  assert.deepEqual(createSignalingRecycleDiagnostic({
    reason: "network-wss-startup-timeout",
    signalingGeneration: 7,
    signalingRegistered: true,
    socketReadyState: 1,
    usingWssMedia: true,
    mediaSocketOpen: true,
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
    mediaSocketOpen: true,
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

test("transport accepts auto, webrtc, wss, and the wsl compatibility alias", () => {
  assert.equal(parseTransport(new URLSearchParams()), "auto");
  assert.equal(parseTransport(new URLSearchParams("transport=webrtc")), "webrtc");
  assert.equal(parseTransport(new URLSearchParams("transport=wss")), "wss");
  assert.equal(parseTransport(new URLSearchParams("wsl=1")), "wss");
  assert.equal(parseTransport(new URLSearchParams("transport=wsl")), "wss");
  assert.equal(parseTransport(new URLSearchParams("transport=unknown")), "auto");
});

test("auto mode remembers an RTC failure in supplied session storage and the query still overrides it", () => {
  const values = new Map();
  const storage = {
    getItem: key => values.get(key) ?? null,
    setItem: (key, value) => values.set(key, value),
    removeItem: key => values.delete(key),
  };
  const now = 1_700_000_000_000;
  assert.equal(rememberedRtcFailure("auto", storage, now), false);
  rememberRtcFailure(storage, now);
  assert.equal(values.get(RTC_FAILURE_STORAGE_KEY), String(now));
  assert.equal(rememberedRtcFailure("auto", storage, now + 60_000), true);
  assert.equal(rememberedRtcFailure("webrtc", storage, now + 60_000), false);
  assert.equal(rememberedRtcFailure("wss", storage, now + 60_000), false);
  assert.equal(rememberedRtcFailure("auto", storage, now + 24 * 60 * 60 * 1000), true);
  forgetRtcFailure(storage);
  assert.equal(rememberedRtcFailure("auto", storage, now + 60_000), false);
  // Storage may be missing or throw in private contexts.
  const throwing = { getItem() { throw new Error("denied"); }, setItem() { throw new Error("denied"); },
    removeItem() { throw new Error("denied"); } };
  assert.doesNotThrow(() => rememberRtcFailure(throwing, now));
  assert.doesNotThrow(() => forgetRtcFailure(throwing));
  assert.equal(rememberedRtcFailure("auto", throwing, now), false);
  assert.equal(rememberedRtcFailure("auto", null, now), false);
  values.set(RTC_FAILURE_STORAGE_KEY, "garbage");
  assert.equal(rememberedRtcFailure("auto", storage, now), false);
});

test("WSS quality defaults high, remembers selection, and lets the query override it", () => {
  const memory = { getItem: () => "balanced" };
  assert.equal(parseQualityPreset(new URLSearchParams()), "high");
  assert.equal(parseQualityPreset(new URLSearchParams(), memory), "balanced");
  assert.equal(parseQualityPreset(new URLSearchParams("quality=saver"), memory), "saver");
  assert.equal(parseQualityPreset(new URLSearchParams("quality=auto"), memory), "auto");
  assert.equal(parseQualityPreset(new URLSearchParams("quality=invalid"), { getItem: () => "invalid" }), "high");
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

test("a connected native WebRTC peer with presented frames never falls back to WSS", () => {
  const healthyPeer = { connected: true, peerConnected: true, statsAvailable: true,
    requestAtMs: 0, lastProgressAtMs: 0, nowMs: 5_000 };
  // Production regression: ICE connected, native HEVC presented 11 frames at
  // 8 fps, yet the 2 s no-rtp timer fired because only the encoded-frame
  // counter was consulted.
  assert.equal(shouldFallbackToWssOnMissingRtp({
    ...healthyPeer,
    framesReceived: 0,
    nativeFramesPresented: 11,
  }), false);
  assert.equal(shouldFallbackToWssOnMissingRtp({
    ...healthyPeer,
    framesReceived: 7,
    nativeFramesPresented: 0,
  }), false);
  assert.equal(shouldFallbackToWssOnMissingRtp({
    ...healthyPeer,
    framesReceived: 0,
    nativeFramesPresented: 0,
  }), true);
  assert.equal(shouldFallbackToWssOnMissingRtp({ connected: false }), false);
  // Unknown counters keep the original rescue behavior for a genuinely silent
  // peer; they must never be read as evidence that media arrived.
  assert.equal(shouldFallbackToWssOnMissingRtp({
    ...healthyPeer,
    framesReceived: null,
    nativeFramesPresented: null,
  }), true);
});

test("the receiver consults native presentation before the automatic WSS fallback", () => {
  const receiver = readFileSync(new URL("../public/receiver.js", import.meta.url), "utf8");
  assert.match(receiver,
    /createRtcStartupFallback\(\{[\s\S]*?nativeFramesPresented: nativePresentation\?\.generation === generation/);
  assert.match(receiver,
    /rtcStartupFallback\?\.stop\(\);[\s\S]{0,400}?markFirstFramePresented\(null, null, \{ rtpTimestamp \}\);/);
});
