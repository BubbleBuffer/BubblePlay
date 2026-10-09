import test from "node:test";
import assert from "node:assert/strict";
import worker, { Room, benchStatusRejectionReason } from "../src/index.js";
import { STREAM_LAB_COMPATIBILITY } from "../public/media.js";

const CURRENT_MEDIA_EPOCH = "0123456789abcdef0123456789abcdef";
const STALE_MEDIA_EPOCH = "fedcba9876543210fedcba9876543210";

class FakeSocket {
  constructor(role) {
    this.attachment = { role };
    this.readyState = 1;
    this.messages = [];
    this.closed = [];
  }

  deserializeAttachment() { return this.attachment; }
  serializeAttachment(value) { this.attachment = value; }
  send(message) { this.messages.push(message); }
  close(code, reason) { this.closed.push({ code, reason }); }
}

function createRoom() {
  const sockets = [new FakeSocket("client"), new FakeSocket("host")];
  sockets[0].attachment.mediaEpoch = CURRENT_MEDIA_EPOCH;
  const storage = { get: async () => [], put: async () => {}, delete: async () => {} };
  return { room: new Room({ getWebSockets: () => sockets, storage }, { TOKEN: "test" }), sockets };
}

test("Worker exposes the immutable lab compatibility descriptor", async () => {
  const response = await worker.fetch(new Request("https://play.example/lab-compat"), {});
  assert.equal(response.status, 200);
  assert.deepEqual(await response.json(), {
    ok: true,
    compatibility: STREAM_LAB_COMPATIBILITY,
  });
});

test("quiescent benchmark validation reports the missing activity field", () => {
  const status = {
    type: "bench-status", protocol: 2, runId: "123e4567-e89b-42d3-a456-426614174000",
    sequence: 9, profileRevision: 5, phase: "sample", implementationVariant: "candidate",
    experimentId: "png-quiescent-detail",
    effective: {
      transport: "wss", codec: "hevc", streamMode: "work", width: 1920, height: 1080,
      fps: 30, bitrateBps: 6_000_000, gopSeconds: 2, implementationVariant: "candidate",
      experimentId: "png-quiescent-detail",
      hybridMode: "png-quiescent-detail",
      hybridCacheMode: "suppress-reactivate",
      encoder: { preset: "p3", tuning: "ultra-low-latency", multipass: "disabled", aqMode: "spatial",
        aqStrength: 8, vbvFrames: 1, lookaheadDepth: 0, weightedPrediction: false,
        dirtyQpMap: false, main10: false, roiMode: "off", resourceMode: "per-frame", asyncDepth: 0 },
    },
  };
  assert.equal(
    benchStatusRejectionReason(status, "host", JSON.stringify(status)),
    "hybrid-activity-required",
  );
  const invalidPolicy = structuredClone(status);
  invalidPolicy.effective.hybridCacheMode = "invented";
  assert.equal(
    benchStatusRejectionReason(invalidPolicy, "host", JSON.stringify(invalidPolicy)),
    "effective-profile",
  );
  const nonQuiescentDda = structuredClone(status);
  nonQuiescentDda.effective.hybridMode = "off";
  nonQuiescentDda.effective.hybridCacheMode = "dda-gated";
  assert.equal(
    benchStatusRejectionReason(nonQuiescentDda, "host", JSON.stringify(nonQuiescentDda)),
    "effective-profile",
  );
});

test("Worker admits a bounded codec-native HEVC tile activity contract", () => {
  const status = {
    type: "bench-status", protocol: 2, runId: "123e4567-e89b-42d3-a456-426614174000",
    sequence: 9, profileRevision: 5, phase: "sample", implementationVariant: "candidate",
    experimentId: "hevc-quiescent-tiles",
    effective: {
      transport: "wss", codec: "hevc", streamMode: "work", width: 1920, height: 1080,
      fps: 30, bitrateBps: 6_000_000, gopSeconds: 2, implementationVariant: "candidate",
      experimentId: "hevc-quiescent-tiles",
      hybridMode: "hevc-quiescent-tiles", hybridCacheMode: "suppress-reactivate",
      encoder: { preset: "p3", tuning: "ultra-low-latency", multipass: "disabled", aqMode: "spatial",
        aqStrength: 8, vbvFrames: 1, lookaheadDepth: 0, weightedPrediction: false,
        dirtyQpMap: false, main10: false, roiMode: "off", resourceMode: "per-frame", asyncDepth: 0 },
    },
    hybridActivity: {
      mode: "hevc-quiescent-tiles", frames: 1, historyResets: 0, historyPreservedFrames: 1,
      motionBypasses: 0, cropRemaps: 0, cursorNeutralFrames: 1, candidates: 1,
      reissuedTiles: 0, upserts: 1, invalidations: 0,
      cache: { s: 0, r: 0, i: 0, x: 0, b: 4096, a: 0 },
      budgetSkippedUpserts: 0, pngBytes: 0, pngUsTotal: 0, pngUsMax: 0,
      hevcTileBytes: 4096, hevcTileCopyUsTotal: 10,
      hevcTileCopyUsMax: 10, hevcTileEncodeUsTotal: 600, hevcTileEncodeUsMax: 600,
      hevcTileEncoderOpens: 1, hevcTileEncoderFailures: 0, sidecarBytes: 4112,
      deferredValidationReadyFrames: 0, deferredValidationPendingFrames: 0,
      damageAuditFrames: 0, damageAuditMetadataUnavailableFrames: 0,
      damageAuditChangedCtbs: 0, damageAuditCoveredCtbs: 0, damageAuditMissedCtbs: 0,
    },
  };

  assert.equal(benchStatusRejectionReason(status, "host", JSON.stringify(status)), null);
});

test("Worker admits only the bounded CQ23 external-budget VBR status", () => {
  const status = {
    type: "bench-status", protocol: 2, runId: "123e4567-e89b-42d3-a456-426614174000",
    sequence: 9, profileRevision: 5, phase: "sample", implementationVariant: "candidate",
    experimentId: "external-budget-vbr-cq23",
    effective: {
      transport: "wss", codec: "hevc", streamMode: "work", width: 1920, height: 1080,
      fps: 30, bitrateBps: 6_000_000, gopSeconds: 2, implementationVariant: "candidate",
      experimentId: "external-budget-vbr-cq23",
      hybridMode: "off", hybridCacheMode: "suppress-reactivate",
      encoder: { preset: "p3", tuning: "ultra-low-latency", multipass: "disabled", aqMode: "spatial",
        aqStrength: 8, vbvFrames: 4, lookaheadDepth: 0, weightedPrediction: false,
        dirtyQpMap: false, main10: false, roiMode: "off", resourceMode: "per-frame", asyncDepth: 0,
        rateControlMode: "external-budget-vbr", targetQuality: 23, peakBitrateMultiplier: 4 },
    },
  };
  assert.equal(benchStatusRejectionReason(status, "host", JSON.stringify(status)), null);
  const invalid = structuredClone(status);
  invalid.effective.encoder.targetQuality = 22;
  assert.equal(benchStatusRejectionReason(invalid, "host", JSON.stringify(invalid)), "effective-profile");
});

test("lab clients receive a bounded benchmark rejection diagnostic", async () => {
  const { room, sockets: [client, host] } = createRoom();
  client.attachment.run = "123e4567-e89b-42d3-a456-426614174000";
  host.attachment.run = client.attachment.run;
  await room.webSocketMessage(host, JSON.stringify({ type: "bench-status" }));
  assert.deepEqual(client.messages.map(message => JSON.parse(message)), [{
    type: "bench-status-rejected",
    protocol: 1,
    reason: "protocol",
  }]);
});

test("client hello persists the minted media epoch on its socket attachment", async () => {
  const client = new FakeSocket(null);
  client.attachment = { role: null, expectedRole: "client", epoch: null };
  const storage = { get: async () => [], put: async () => {}, delete: async () => {} };
  const room = new Room({ getWebSockets: () => [client], storage }, { TOKEN: "test" });

  await room.webSocketMessage(client, JSON.stringify({ type: "hello", role: "client" }));

  const hello = JSON.parse(client.messages[0]);
  assert.match(hello.mediaEpoch, /^[a-f0-9]{32}$/);
  assert.equal(client.attachment.mediaEpoch, hello.mediaEpoch);
});

test("a client registering after the host receives the host's latest capabilities", async () => {
  const host = new FakeSocket("host");
  const client = new FakeSocket(null);
  client.attachment = { role: null, expectedRole: "client", epoch: null };
  const sockets = [host, client];
  const storage = { get: async () => [], put: async () => {}, delete: async () => {} };
  const room = new Room({ getWebSockets: () => sockets, storage }, { TOKEN: "test" });
  const capability = JSON.stringify({ type: "host-capabilities", serviceMode: false, coldBoot: false,
    secureDesktop: false, secureText: false });

  await room.webSocketMessage(host, capability);
  await room.webSocketMessage(client, JSON.stringify({ type: "hello", role: "client" }));

  assert.equal(client.messages.length, 2);
  assert.equal(JSON.parse(client.messages[0]).type, "hello-ok");
  assert.equal(client.messages[1], capability);
});

test("relays bounded QoS client-to-host and quality host-to-client only", async () => {
  const { room, sockets: [client, host] } = createRoom();
  const qos = JSON.stringify({ type: "qos", sessionId: "page-random", seq: 1, rx: { mbps: 4.2 } });
  const quality = JSON.stringify({ type: "quality", profile: "balanced", width: 1280, height: 720, fps: 30 });

  await room.webSocketMessage(client, qos);
  await room.webSocketMessage(host, quality);

  assert.deepEqual(host.messages, [qos]);
  assert.deepEqual(client.messages, [quality]);
  assert.deepEqual(client.closed, []);
  assert.deepEqual(host.closed, []);
});

test("acknowledges signaling keepalives on the same socket", async () => {
  const { room, sockets: [client, host] } = createRoom();

  await room.webSocketMessage(host, JSON.stringify({ type: "ping" }));

  assert.deepEqual(host.messages, [JSON.stringify({ type: "pong" })]);
  assert.deepEqual(client.messages, []);
});

test("drops wrong-role and oversize QoS/quality frames without closing peers", async () => {
  const { room, sockets: [client, host] } = createRoom();
  const oversizeQos = JSON.stringify({ type: "qos", payload: "x".repeat(2_048) });
  const oversizeQuality = JSON.stringify({ type: "quality", payload: "x".repeat(1_024) });

  await room.webSocketMessage(host, JSON.stringify({ type: "qos" }));
  await room.webSocketMessage(client, JSON.stringify({ type: "quality" }));
  await room.webSocketMessage(client, oversizeQos);
  await room.webSocketMessage(host, oversizeQuality);

  assert.deepEqual(client.messages, []);
  assert.deepEqual(host.messages, []);
  assert.deepEqual(client.closed, []);
  assert.deepEqual(host.closed, []);
});

test("relays only bounded versioned benchmark telemetry from the host", async () => {
  const { room, sockets: [client, host] } = createRoom();
  const sample = JSON.stringify({
    type: "bench-status", protocol: 1, runId: "123e4567-e89b-42d3-a456-426614174000",
    sequence: 7, profileRevision: 3, phase: "sample",
    effective: {
      transport: "wss", codec: "hevc", streamMode: "work", width: 1920, height: 1080,
      fps: 30, bitrateBps: 4_000_000, gopSeconds: 2,
      encoder: { preset: "p3", tuning: "ultra-low-latency", multipass: "disabled", aqStrength: 8, temporalAq: false, vbvFrames: 1 },
    },
    metrics: { encodedBytes: 1234, wssPayloadBytes: 1678, encodeP95Us: 2100 },
  });
  assert.ok(Buffer.byteLength(sample) <= 2_048);

  await room.webSocketMessage(host, sample);
  await room.webSocketMessage(client, sample);
  const legacy = JSON.parse(sample);
  delete legacy.profileRevision;
  await room.webSocketMessage(host, JSON.stringify(legacy));
  await room.webSocketMessage(host, JSON.stringify({ type: "bench-status", protocol: 2, runId: "bad", seq: 1, phase: "sample" }));
  await room.webSocketMessage(host, JSON.stringify({ type: "bench-status", protocol: 1, runId: "123e4567-e89b-42d3-a456-426614174000", seq: 8, phase: "unknown" }));
  await room.webSocketMessage(host, JSON.stringify({
    type: "bench-status", protocol: 1, runId: "123e4567-e89b-42d3-a456-426614174000",
    sequence: 8, profileRevision: 4, phase: "sample", effective: { transport: "wss", codec: "hevc" },
  }));
  await room.webSocketMessage(host, JSON.stringify({ type: "bench-status", payload: "x".repeat(2_048) }));

  assert.deepEqual(client.messages, [sample]);
  assert.deepEqual(host.messages, []);
  assert.deepEqual(client.closed, []);
  assert.deepEqual(host.closed, []);
});

test("relays strict v2 candidate benchmark telemetry while retaining v1 compatibility", async () => {
  const { room, sockets: [client, host] } = createRoom();
  const v2 = {
    type: "bench-status", protocol: 2, runId: "123e4567-e89b-42d3-a456-426614174000",
    sequence: 8, profileRevision: 4, phase: "sample", implementationVariant: "candidate",
    experimentId: "enc-work-001",
    effective: {
      transport: "wss", codec: "hevc", streamMode: "work", width: 1920, height: 1080,
      fps: 30, bitrateBps: 6_000_000, gopSeconds: 2, implementationVariant: "candidate",
      experimentId: "enc-work-001",
      hybridMode: "off",
      encoder: { preset: "p7", tuning: "low-latency", multipass: "full", aqMode: "spatial",
        aqStrength: 15, vbvFrames: 4, lookaheadDepth: 2, weightedPrediction: true, dirtyQpMap: true, main10: false,
        roiMode: "damage", resourceMode: "per-frame", asyncDepth: 0 },
    },
    roiActivity: {
      mode: "damage", evaluatedFrames: 31, mappedFrames: 29, missingDamageFrames: 0,
      emptyDamageFrames: 0, idrFrames: 1, coverageRejectedFrames: 1, unsupportedFrames: 0,
      lastSequence: 4_294_967_295, lastReason: "coverage-at-least-half",
      lastChangedCtbs: 2_039, lastTotalCtbs: 2_040,
      lastMinDelta: -3, lastMaxDelta: 1,
    },
    metrics: {
      temporalRoi: {
        source: "gpu-temporal", temporalFrames: 31, temporalPrimingFrames: 1,
        analysisP95Us: 1_499, analysisMaxUs: 59_876,
      },
    },
    encodeStages: {
      frames: 31,
      register: { count: 31, totalUs: 1_234_567, maxUs: 98_765 },
      map: { count: 31, totalUs: 1_234_567, maxUs: 98_765 },
      submit: { count: 31, totalUs: 12_345_678, maxUs: 987_654 },
      lock: { count: 31, totalUs: 12_345_678, maxUs: 987_654 },
      copy: { count: 31, totalUs: 1_234_567, maxUs: 98_765 },
      unmap: { count: 31, totalUs: 1_234_567, maxUs: 98_765 },
      unregister: { count: 31, totalUs: 1_234_567, maxUs: 98_765 },
    },
    windowDurationMs: 1_007,
    encodedBytes: 9_876_543,
    wssPayloadBytes: 12_345_678,
    encodeCount: 31,
    encodeUsTotal: 9_876_543,
    encodeUsMax: 654_321,
    idrCount: 1,
    deltaCount: 30,
    skips: 12_345,
    qpMapFrames: 29,
    qpMapChangedCtbs: 59_160,
    qpMapTotalCtbs: 63_240,
  };
  const valid = JSON.stringify(v2);
  assert.ok(Buffer.byteLength(valid) <= 2_048);

  await room.webSocketMessage(host, valid);
  await room.webSocketMessage(host, JSON.stringify({ ...v2, experimentId: undefined }));
  await room.webSocketMessage(host, JSON.stringify({ ...v2, effective: { ...v2.effective, encoder: {
    ...v2.effective.encoder, lookaheadDepth: 3,
  } } }));
  await room.webSocketMessage(host, JSON.stringify({ ...v2, roiActivity: {
    ...v2.roiActivity, lastReason: "invented",
  } }));
  await room.webSocketMessage(host, JSON.stringify({ ...v2, roiActivity: {
    ...v2.roiActivity, temporalPrimingFrames: 32,
  } }));
  await room.webSocketMessage(host, JSON.stringify({ ...v2, encodeStages: {
    ...v2.encodeStages, map: { ...v2.encodeStages.map, count: 1 },
  } }));
  await room.webSocketMessage(host, JSON.stringify({ ...v2, encodeStages: {
    ...v2.encodeStages, unregister: { count: 0, totalUs: 1, maxUs: 0 },
  } }));
  await room.webSocketMessage(host, JSON.stringify({ ...v2, encodeStages: {
    ...v2.encodeStages, copy: { ...v2.encodeStages.copy, extra: 1 },
  } }));
  await room.webSocketMessage(host, JSON.stringify({ ...v2, protocol: 1 }));

  assert.deepEqual(client.messages, [valid]);
});

test("relays reconstruction-feedback activity only for a feedback ROI candidate", async () => {
  const { room, sockets: [client, host] } = createRoom();
  const status = {
    type: "bench-status", protocol: 2, runId: "123e4567-e89b-42d3-a456-426614174000",
    sequence: 9, profileRevision: 5, phase: "sample", implementationVariant: "candidate",
    experimentId: "feedback-roi-001",
    effective: {
      transport: "wss", codec: "hevc", streamMode: "work", width: 1920, height: 1080,
      fps: 30, bitrateBps: 6_000_000, gopSeconds: 2, implementationVariant: "candidate",
      experimentId: "feedback-roi-001",
      hybridMode: "off", hybridCacheMode: "suppress-reactivate",
      encoder: { preset: "p3", tuning: "ultra-low-latency", multipass: "disabled",
        aqMode: "spatial", aqStrength: 8, vbvFrames: 1, lookaheadDepth: 0,
        weightedPrediction: false, dirtyQpMap: false, main10: false,
        roiMode: "feedback", resourceMode: "per-frame", asyncDepth: 0 },
    },
    roiActivity: {
      mode: "feedback", evaluatedFrames: 31, mappedFrames: 20, missingDamageFrames: 9,
      emptyDamageFrames: 0, idrFrames: 2, coverageRejectedFrames: 0, unsupportedFrames: 0,
      source: "gpu-temporal", temporalFrames: 31, temporalPrimingFrames: 1,
      analysisP95Us: 900, analysisMaxUs: 1_100,
      reconstructionActive: true, feedbackSubmittedFrames: 30, feedbackReadyFrames: 29,
      feedbackLateFrames: 1, feedbackSkippedFrames: 0, feedbackAdjustedFrames: 20,
      feedbackWarmupFrames: 9, feedbackMotionBypasses: 0, feedbackMissingFrames: 2,
      feedbackEligibleCtbs: 160, feedbackRefinedCtbs: 20, feedbackFundedCtbs: 40,
      feedbackSubmitP95Us: 80, feedbackSubmitMaxUs: 120,
      feedbackReadbackP95Us: 60, feedbackReadbackMaxUs: 90,
      lastFeedbackSequence: 30, lastFeedbackAgeFrames: 1,
      lastSequence: 31, lastReason: "applied", lastChangedCtbs: 20,
      lastTotalCtbs: 2_040, lastMinDelta: -2, lastMaxDelta: 1,
    },
    windowDurationMs: 1_000, encodedBytes: 700_000, wssPayloadBytes: 780_000,
    encodeCount: 31, encodeUsTotal: 31_000, encodeUsMax: 1_200,
    idrCount: 2, deltaCount: 29, skips: 0, qpMapFrames: 20,
    qpMapChangedCtbs: 400, qpMapTotalCtbs: 40_800,
  };
  const valid = JSON.stringify(status);
  assert.ok(Buffer.byteLength(valid) <= 2_048);

  await room.webSocketMessage(host, valid);
  await room.webSocketMessage(host, JSON.stringify({ ...status, roiActivity: {
    ...status.roiActivity, feedbackAdjustedFrames: 32,
  } }));
  await room.webSocketMessage(host, JSON.stringify({ ...status, roiActivity: {
    ...status.roiActivity, reconstructionActive: "yes",
  } }));

  assert.deepEqual(client.messages, [valid]);
});

test("relays feedback-headroom only with exact wire-occupancy policy telemetry", async () => {
  const { room, sockets: [client, host] } = createRoom();
  const status = {
    type: "bench-status", protocol: 2, runId: "123e4567-e89b-42d3-a456-426614174000",
    sequence: 9, profileRevision: 5, phase: "sample", implementationVariant: "candidate",
    experimentId: "feedback-headroom-001",
    effective: {
      transport: "wss", codec: "hevc", streamMode: "work", width: 1920, height: 1080,
      fps: 30, bitrateBps: 6_000_000, gopSeconds: 2, implementationVariant: "candidate",
      experimentId: "feedback-headroom-001",
      hybridMode: "off", hybridCacheMode: "suppress-reactivate",
      encoder: { preset: "p3", tuning: "ultra-low-latency", multipass: "disabled",
        aqMode: "spatial", aqStrength: 8, vbvFrames: 1, lookaheadDepth: 0,
        weightedPrediction: false, dirtyQpMap: false, main10: false,
        roiMode: "feedback-headroom", resourceMode: "per-frame", asyncDepth: 0 },
    },
    roiActivity: {
      mode: "feedback-headroom", evaluatedFrames: 31, mappedFrames: 20,
      missingDamageFrames: 9, emptyDamageFrames: 0, idrFrames: 2,
      coverageRejectedFrames: 0, unsupportedFrames: 0, source: "gpu-temporal",
      temporalFrames: 31, temporalPrimingFrames: 1, analysisP95Us: 900, analysisMaxUs: 1_100,
      reconstructionActive: true, feedbackSubmittedFrames: 30, feedbackReadyFrames: 29,
      feedbackLateFrames: 1, feedbackSkippedFrames: 0, feedbackAdjustedFrames: 20,
      feedbackWarmupFrames: 9, feedbackMotionBypasses: 0, feedbackMissingFrames: 2,
      feedbackEligibleCtbs: 160, feedbackRefinedCtbs: 20, feedbackFundedCtbs: 4,
      feedbackSpendHeadroomFrames: 18, feedbackNetZeroFrames: 2,
      feedbackWireOccupancyP95Bps: 2_100_000, feedbackWireOccupancyMaxBps: 2_400_000,
      feedbackSubmitP95Us: 80, feedbackSubmitMaxUs: 120,
      feedbackReadbackP95Us: 60, feedbackReadbackMaxUs: 90,
      lastFeedbackSequence: 30, lastFeedbackAgeFrames: 1,
      lastFeedbackBudgetPolicy: "spend-headroom", lastFeedbackWireOccupancyBps: 1_900_000,
      lastSequence: 31, lastReason: "applied", lastChangedCtbs: 20,
      lastTotalCtbs: 2_040, lastMinDelta: -5, lastMaxDelta: 0,
    },
    windowDurationMs: 1_000, encodedBytes: 700_000, wssPayloadBytes: 780_000,
    encodeCount: 31, encodeUsTotal: 31_000, encodeUsMax: 1_200,
    idrCount: 2, deltaCount: 29, skips: 0, qpMapFrames: 20,
    qpMapChangedCtbs: 400, qpMapTotalCtbs: 40_800,
  };
  const valid = JSON.stringify(status);
  assert.ok(Buffer.byteLength(valid) <= 4_096);
  assert.equal(benchStatusRejectionReason(status, "host", valid), null);
  await room.webSocketMessage(host, valid);
  const { feedbackSpendHeadroomFrames: _missing, ...withoutActivity } = status.roiActivity;
  await room.webSocketMessage(host, JSON.stringify({ ...status, roiActivity: withoutActivity }));
  await room.webSocketMessage(host, JSON.stringify({ ...status, roiActivity: {
    ...status.roiActivity, lastFeedbackBudgetPolicy: "unbounded",
  } }));

  assert.deepEqual(client.messages, [valid]);
});

test("relays the bandwidth-shaped quiescence-gated hybrid experiment profile", async () => {
  const { room, sockets: [client, host] } = createRoom();
  const status = {
    type: "bench-status", protocol: 2, runId: "123e4567-e89b-42d3-a456-426614174000",
    sequence: 9, profileRevision: 5, phase: "sample", implementationVariant: "candidate",
    experimentId: "png-quiescent-detail",
    effective: {
      transport: "wss", codec: "hevc", streamMode: "work", width: 1920, height: 1080,
      fps: 30, bitrateBps: 5_700_000, gopSeconds: 2, implementationVariant: "candidate",
      experimentId: "png-quiescent-detail",
      hybridMode: "png-quiescent-detail",
      hybridCacheMode: "suppress-reactivate",
      encoder: { preset: "p3", tuning: "ultra-low-latency", multipass: "disabled", aqMode: "spatial",
        aqStrength: 8, vbvFrames: 1, lookaheadDepth: 0, weightedPrediction: false,
        dirtyQpMap: false, main10: false, roiMode: "off", resourceMode: "per-frame", asyncDepth: 0 },
    },
    hybridActivity: {
      mode: "png-quiescent-detail", frames: 1, historyResets: 0, historyPreservedFrames: 1,
      motionBypasses: 0, cropRemaps: 0, cursorNeutralFrames: 1, candidates: 0,
      reissuedTiles: 0, upserts: 0, invalidations: 0,
      cache: { s: 1, r: 1, i: 0, x: 0, b: 12_345, a: 12_345 }, budgetSkippedUpserts: 0,
      pngBytes: 0, pngUsTotal: 0, pngUsMax: 0, sidecarBytes: 0,
      validationFrames: 1, validationUnavailableFrames: 0, validationFailedFrames: 0,
      deferredValidationReadyFrames: 1, deferredValidationPendingFrames: 0,
      deferredValidationAgeP95Frames: 1, deferredValidationAgeMaxFrames: 1,
      deferredValidationPendingMaxAgeFrames: 0,
      damageAuditFrames: 1, damageAuditMetadataUnavailableFrames: 0,
      damageAuditChangedCtbs: 2, damageAuditCoveredCtbs: 2, damageAuditMissedCtbs: 0,
      validationSubmitP95Us: 120, validationSubmitMaxUs: 150,
      validationCollectP95Us: 12, validationCollectMaxUs: 18, lastValidationSequence: 41,
    },
  };
  const wire = JSON.stringify(status);
  assert.ok(Buffer.byteLength(wire) <= 2_048);

  await room.webSocketMessage(host, wire);
  await room.webSocketMessage(host, JSON.stringify({
    ...status,
    effective: { ...status.effective, bitrateBps: 6_000_001 },
  }));
  await room.webSocketMessage(host, JSON.stringify({
    ...status,
    effective: { ...status.effective, hybridMode: "png-invented-detail" },
  }));
  await room.webSocketMessage(host, JSON.stringify({
    ...status,
    hybridActivity: { ...status.hybridActivity, mode: "png-static-roi" },
  }));
  await room.webSocketMessage(host, JSON.stringify({
    ...status,
    hybridActivity: { ...status.hybridActivity, analysisP95Us: 1_501, analysisMaxUs: 1_500 },
  }));
  await room.webSocketMessage(host, JSON.stringify({
    ...status,
    hybridActivity: { ...status.hybridActivity, validationUnavailableFrames: 2 },
  }));
  await room.webSocketMessage(host, JSON.stringify({
    ...status,
    hybridActivity: { ...status.hybridActivity, deferredValidationAgeP95Frames: 2 },
  }));
  await room.webSocketMessage(host, JSON.stringify({
    ...status,
    hybridActivity: { ...status.hybridActivity, damageAuditMetadataUnavailableFrames: 2 },
  }));
  await room.webSocketMessage(host, JSON.stringify({
    ...status,
    hybridActivity: { ...status.hybridActivity, damageAuditCoveredCtbs: 1 },
  }));
  const { damageAuditFrames: _omitted, ...missingDamageAuditFrames } = status.hybridActivity;
  await room.webSocketMessage(host, JSON.stringify({
    ...status,
    hybridActivity: missingDamageAuditFrames,
  }));

  assert.deepEqual(client.messages, [wire]);
});

test("relays completed benchmark stages that round below one microsecond", async () => {
  const { room, sockets: [client, host] } = createRoom();
  const status = {
    type: "bench-status", protocol: 2, runId: "123e4567-e89b-42d3-a456-426614174000",
    sequence: 10, profileRevision: 5, phase: "sample", implementationVariant: "candidate",
    experimentId: "png-quiescent-detail",
    effective: {
      transport: "wss", codec: "hevc", streamMode: "work", width: 1920, height: 1080,
      fps: 30, bitrateBps: 6_000_000, gopSeconds: 2, implementationVariant: "candidate",
      experimentId: "png-quiescent-detail",
      hybridMode: "png-quiescent-detail",
      hybridCacheMode: "suppress-reactivate",
      encoder: { preset: "p3", tuning: "ultra-low-latency", multipass: "disabled", aqMode: "spatial",
        aqStrength: 8, vbvFrames: 1, lookaheadDepth: 0, weightedPrediction: false,
        dirtyQpMap: false, main10: false, roiMode: "off", resourceMode: "per-frame", asyncDepth: 0 },
    },
    encodeStages: {
      frames: 1,
      register: { count: 1, totalUs: 51, maxUs: 51 },
      map: { count: 1, totalUs: 2, maxUs: 2 },
      submit: { count: 1, totalUs: 383, maxUs: 383 },
      lock: { count: 1, totalUs: 5_935, maxUs: 5_935 },
      copy: { count: 1, totalUs: 7, maxUs: 7 },
      unmap: { count: 1, totalUs: 0, maxUs: 0 },
      unregister: { count: 1, totalUs: 1, maxUs: 1 },
    },
    hybridActivity: {
      mode: "png-quiescent-detail", frames: 1, historyResets: 0, historyPreservedFrames: 1,
      motionBypasses: 0, cropRemaps: 1, cursorNeutralFrames: 0, candidates: 4,
      reissuedTiles: 4, upserts: 0, invalidations: 0,
      cache: { s: 0, r: 0, i: 0, x: 0, b: 0, a: 0 }, budgetSkippedUpserts: 4,
      pngBytes: 0, pngUsTotal: 15_954, pngUsMax: 10_913,
      analysisP95Us: 3_650, analysisMaxUs: 3_650, sidecarBytes: 4,
      deferredValidationReadyFrames: 0, deferredValidationPendingFrames: 0,
      damageAuditFrames: 0, damageAuditMetadataUnavailableFrames: 0,
      damageAuditChangedCtbs: 0, damageAuditCoveredCtbs: 0, damageAuditMissedCtbs: 0,
    },
  };
  const wire = JSON.stringify(status);

  await room.webSocketMessage(host, wire);

  assert.deepEqual(client.messages, [wire]);
});

test("relays a v2 shaped effective bitrate below the experiment search floor", async () => {
  const { room, sockets: [client, host] } = createRoom();
  const status = {
    type: "bench-status", protocol: 2, runId: "123e4567-e89b-42d3-a456-426614174000",
    sequence: 20, profileRevision: 12, phase: "sample", implementationVariant: "candidate",
    experimentId: "shaped-2m",
    effective: {
      transport: "wss", codec: "hevc", streamMode: "work", width: 1920, height: 1080,
      fps: 30, bitrateBps: 500_000, gopSeconds: 2, implementationVariant: "candidate",
      experimentId: "shaped-2m",
      encoder: { preset: "p3", tuning: "ultra-low-latency", multipass: "disabled", aqMode: "spatial",
        aqStrength: 8, vbvFrames: 1, lookaheadDepth: 0, weightedPrediction: false, dirtyQpMap: false, main10: false },
    },
  };
  const wire = JSON.stringify(status);
  await room.webSocketMessage(host, wire);
  assert.deepEqual(client.messages, [wire]);
});

test("drops benchmark statuses with invalid revisions or effective profile values", async () => {
  const { room, sockets: [client, host] } = createRoom();
  const base = {
    type: "bench-status", protocol: 1, runId: "123e4567-e89b-42d3-a456-426614174000",
    sequence: 7, profileRevision: 3, phase: "sample",
    effective: {
      transport: "wss", codec: "hevc", streamMode: "game", width: 1920, height: 1080,
      fps: 60, bitrateBps: 8_000_000, gopSeconds: 1,
      encoder: { preset: "p5", tuning: "low-latency", multipass: "quarter", aqStrength: 15, temporalAq: true, vbvFrames: 4 },
    },
  };
  const invalid = [
    { ...base, profileRevision: -1 },
    { ...base, profileRevision: Number.MAX_SAFE_INTEGER + 1 },
    { ...base, effective: { ...base.effective, transport: "udp" } },
    { ...base, effective: { ...base.effective, codec: "h264" } },
    { ...base, effective: { ...base.effective, streamMode: "work", fps: 60 } },
    { ...base, effective: { ...base.effective, width: 319 } },
    { ...base, effective: { ...base.effective, height: 2161 } },
    { ...base, effective: { ...base.effective, bitrateBps: 40_000_001 } },
    { ...base, effective: { ...base.effective, gopSeconds: 3 } },
    { ...base, effective: { ...base.effective, encoder: { ...base.effective.encoder, preset: "p2" } } },
    { ...base, effective: { ...base.effective, encoder: { ...base.effective.encoder, tuning: "hq" } } },
    { ...base, effective: { ...base.effective, encoder: { ...base.effective.encoder, multipass: "full" } } },
    { ...base, effective: { ...base.effective, encoder: { ...base.effective.encoder, aqStrength: 16 } } },
    { ...base, effective: { ...base.effective, encoder: { ...base.effective.encoder, temporalAq: 1 } } },
    { ...base, effective: { ...base.effective, encoder: { ...base.effective.encoder, vbvFrames: 0 } } },
  ];

  for (const status of invalid) await room.webSocketMessage(host, JSON.stringify(status));

  assert.deepEqual(client.messages, []);
  assert.deepEqual(host.messages, []);
  assert.deepEqual(client.closed, []);
  assert.deepEqual(host.closed, []);
});

test("relays bounded client runtime logs to the host only", async () => {
  const { room, sockets: [client, host] } = createRoom();
  const log = JSON.stringify({ type: "client-log", seq: 1, atMs: 2, level: "error", event: "decoder-error", detail: { message: "unsupported" } });
  await room.webSocketMessage(client, log);
  await room.webSocketMessage(host, log);
  assert.deepEqual(host.messages, [log]);
  assert.deepEqual(client.messages, []);
  assert.deepEqual(client.closed, []);
  assert.deepEqual(host.closed, []);
});

test("relays host desktop availability state to the client", async () => {
  const { room, sockets: [client, host] } = createRoom();
  const state = JSON.stringify({ type: "desktop-state", state: "unavailable", generation: 4, reason: "capture-unavailable" });
  await room.webSocketMessage(host, state);
  assert.deepEqual(client.messages, [state]);
  assert.deepEqual(host.messages, []);
  assert.deepEqual(client.closed, []);
});

test("relays only bounded applied-input receipts from the host", async () => {
  const { room, sockets: [client, host] } = createRoom();
  const status = JSON.stringify({
    type: "input-status", protocol: 1, generation: 4,
    receivedPackets: 12, acceptedPackets: 11, failedPackets: 1, droppedPackets: 1,
    releasedInputs: 2, requestedEvents: 14, appliedEvents: 13,
    lastErrorCode: 5, lastSequence: 99,
  });
  await room.webSocketMessage(host, status);
  await room.webSocketMessage(client, status);
  await room.webSocketMessage(host, JSON.stringify({
    type: "input-status", protocol: 1, generation: 4,
    receivedPackets: 1, acceptedPackets: 2, failedPackets: 0, droppedPackets: 0,
    releasedInputs: 0, requestedEvents: 1, appliedEvents: 2,
  }));
  assert.deepEqual(client.messages, [status]);
  assert.deepEqual(host.messages, []);
});

test("relays additive input warnings and explicit clears, rejecting invalid enums", async () => {
  const { room, sockets: [client, host] } = createRoom();
  const receipt = { type: "input-status", protocol: 1, generation: 4,
    receivedPackets: 0, acceptedPackets: 0, failedPackets: 0, droppedPackets: 0,
    releasedInputs: 0, requestedEvents: 0, appliedEvents: 0 };
  const accepted = [];
  for (const [reason, route] of [["admin-required", "local"], ["admin-unavailable", "admin"], [null, "secure"]]) {
    const message = JSON.stringify({ ...receipt, reason, route });
    accepted.push(message);
    await room.webSocketMessage(host, message);
  }
  await room.webSocketMessage(host, JSON.stringify({ ...receipt, reason: "other", route: "local" }));
  await room.webSocketMessage(host, JSON.stringify({ ...receipt, reason: null, route: "other" }));
  assert.deepEqual(client.messages, accepted);
  assert.deepEqual(host.messages, []);
});

test("relays only bounded privileged capability and secure desktop state from the host", async () => {
  const { room, sockets: [client, host] } = createRoom();
  const capability = JSON.stringify({ type: "host-capabilities", serviceMode: true, coldBoot: true, secureDesktop: true, secureText: true });
  const secure = JSON.stringify({ type: "desktop-state", state: "active", desktop: "secure", input: "enabled", generation: 5, reason: "desktop-switched" });
  await room.webSocketMessage(host, capability);
  await room.webSocketMessage(host, secure);
  await room.webSocketMessage(client, capability);
  await room.webSocketMessage(host, JSON.stringify({ type: "host-capabilities", detail: "x".repeat(2_048) }));
  assert.deepEqual(client.messages, [capability, secure]);
  assert.deepEqual(host.messages, []);
});

test("host capabilities relay only the bounded boolean contract", async () => {
  const { room, sockets: [client, host] } = createRoom();
  const capability = JSON.stringify({ type: "host-capabilities", serviceMode: true, coldBoot: true,
    secureDesktop: true, secureText: true });
  await room.webSocketMessage(host, capability);
  await room.webSocketMessage(host, JSON.stringify({ type: "host-capabilities", serviceMode: "yes",
    coldBoot: true, secureDesktop: true, secureText: true }));
  await room.webSocketMessage(client, capability);
  assert.deepEqual(client.messages, [capability]);
  assert.deepEqual(host.messages, []);
});

test("relays monitor lists and bounded monitor selections", async () => {
  const { room, sockets: [client, host] } = createRoom();
  const list = JSON.stringify({ type: "monitor-list", monitors: [{ selector: "\\\\.\\\\DISPLAY1", name: "Primary", width: 1920, height: 1080, selected: true }] });
  const select = JSON.stringify({ type: "media-control", action: "monitor", protocol: 4, epoch: CURRENT_MEDIA_EPOCH, monitor: "\\\\.\\\\DISPLAY2" });
  await room.webSocketMessage(host, list);
  await room.webSocketMessage(client, select);
  await room.webSocketMessage(client, JSON.stringify({ type: "media-control", action: "monitor", monitor: "x".repeat(129) }));
  assert.deepEqual(client.messages, [list]);
  assert.deepEqual(host.messages, [select]);
  assert.deepEqual(client.closed, []);
  assert.deepEqual(host.closed, []);
});

test("relays only v4 epoch-scoped idempotent monitor refresh requests", async () => {
  const { room, sockets: [client, host] } = createRoom();
  const epoch = CURRENT_MEDIA_EPOCH;
  const refresh = JSON.stringify({ type: "media-control", action: "monitors", protocol: 4, epoch });
  const replay = JSON.stringify({ type: "media-control", action: "monitors", protocol: 4, epoch: STALE_MEDIA_EPOCH });
  await room.webSocketMessage(client, refresh);
  await room.webSocketMessage(client, replay);
  await room.webSocketMessage(client, JSON.stringify({ type: "media-control", action: "monitors", protocol: 2, epoch }));
  await room.webSocketMessage(client, JSON.stringify({ type: "media-control", action: "monitors", protocol: 4 }));
  await room.webSocketMessage(client, JSON.stringify({ type: "media-control", action: "monitors", protocol: 4, epoch: "A".repeat(32) }));
  await room.webSocketMessage(host, refresh);
  assert.deepEqual(host.messages, [refresh]);
  assert.deepEqual(client.messages, []);
  assert.deepEqual(client.closed, []);
  assert.deepEqual(host.closed, []);
});

test("buffers monitor refresh only for the client socket's current media epoch", async () => {
  const client = new FakeSocket("client");
  client.attachment.mediaEpoch = CURRENT_MEDIA_EPOCH;
  let pending = [];
  const storage = {
    get: async () => pending,
    put: async (_key, value) => { pending = value; },
    delete: async () => { pending = []; },
  };
  const room = new Room({ getWebSockets: () => [client], storage }, { TOKEN: "test" });
  const refresh = JSON.stringify({ type: "media-control", action: "monitors", protocol: 4, epoch: CURRENT_MEDIA_EPOCH });
  const replay = JSON.stringify({ type: "media-control", action: "monitors", protocol: 4, epoch: STALE_MEDIA_EPOCH });

  await room.webSocketMessage(client, refresh);
  await room.webSocketMessage(client, replay);

  assert.deepEqual(pending, [refresh]);
});

test("host capabilities relay followWindow only as an integer 0..255", async () => {
  const { room, sockets: [client, host] } = createRoom();
  const base = { type: "host-capabilities", serviceMode: false, coldBoot: false, secureDesktop: false, secureText: false };
  const valid = [0, 1, 255].map(followWindow => JSON.stringify({ ...base, followWindow }));
  for (const text of valid) await room.webSocketMessage(host, text);
  for (const followWindow of [-1, 256, 1.5, "1", true, null])
    await room.webSocketMessage(host, JSON.stringify({ ...base, followWindow }));
  assert.deepEqual(client.messages, valid);
  assert.deepEqual(host.messages, []);
});

test("native file drops require the compatible file transfer capability", async () => {
  const { room, sockets: [client, host] } = createRoom();
  const base = { type: "host-capabilities", serviceMode: false, coldBoot: false, secureDesktop: false, secureText: false };
  const valid = JSON.stringify({ ...base, fileTransfer: 1, fileDrop: 1 });
  await room.webSocketMessage(host, valid);
  for (const fileDrop of [0, 2, "1", true, null])
    await room.webSocketMessage(host, JSON.stringify({ ...base, fileTransfer: 1, fileDrop }));
  await room.webSocketMessage(host, JSON.stringify({ ...base, fileDrop: 1 }));
  await room.webSocketMessage(host, JSON.stringify({ ...base, fileTransfer: 2, fileDrop: 1 }));
  assert.deepEqual(client.messages, [valid]);
});

test("native drag previews require fileTransfer and fileDrop v1 on a local host", async () => {
  const { room, sockets: [client, host] } = createRoom();
  const base = { type: "host-capabilities", serviceMode: false, coldBoot: false, secureDesktop: false, secureText: false };
  const valid = JSON.stringify({ ...base, fileTransfer: 1, fileDrop: 1, fileDropPreview: 1 });
  await room.webSocketMessage(host, valid);
  for (const fileDropPreview of [0, 2, "1", true, null])
    await room.webSocketMessage(host, JSON.stringify({ ...base, fileTransfer: 1, fileDrop: 1, fileDropPreview }));
  for (const change of [{ fileTransfer: 2 }, { fileDrop: 2 }, { fileTransfer: undefined }, { fileDrop: undefined }, { serviceMode: true }])
    await room.webSocketMessage(host, JSON.stringify({ ...JSON.parse(valid), ...change }));
  await room.webSocketMessage(client, valid);
  assert.deepEqual(client.messages, [valid]);
  assert.deepEqual(host.messages, []);
});

function fileDragPreview(change = {}) {
  return { type: "file-drag-preview", protocol: 1, epoch: CURRENT_MEDIA_EPOCH, dragId: 1, action: "update",
    drop: { x: 0, y: 65535, generation: 1, viewportRevision: 0 }, types: ["text/plain", ""], ...change };
}

test("relays current-epoch native drag preview updates and cancellation client-to-host only", async () => {
  const { room, sockets: [client, host] } = createRoom();
  const update = JSON.stringify(fileDragPreview());
  const cancel = JSON.stringify(fileDragPreview({ action: "cancel", drop: undefined, types: undefined }));
  await room.webSocketMessage(client, update);
  await room.webSocketMessage(client, cancel);
  await room.webSocketMessage(host, update);
  await room.webSocketMessage(host, cancel);
  assert.deepEqual(host.messages, [update, cancel]);
  assert.deepEqual(client.messages, []);
  assert.deepEqual(client.closed, []);
  assert.deepEqual(host.closed, []);
});

test("rejects malformed drag preview controls without closing either peer", async () => {
  const { room, sockets: [client, host] } = createRoom();
  for (const change of [
    { epoch: STALE_MEDIA_EPOCH }, { epoch: CURRENT_MEDIA_EPOCH.toUpperCase() }, { epoch: undefined },
    { protocol: 2 }, { protocol: "1" }, { dragId: 0 }, { dragId: -1 }, { dragId: 1.5 },
    { dragId: 0x1_0000_0000 }, { dragId: "1" }, { action: "drop" }, { drop: undefined },
    { drop: null }, { drop: [] }, { types: undefined }, { types: [] }, { types: Array(65).fill("") },
    { types: ["a".repeat(97)] }, { types: ["text/ü"] }, { types: ["bad type"] },
    { types: ["text/\n"] }, { types: [1] }, { extra: true },
    { action: "cancel" }, { action: "cancel", drop: null, types: null },
  ]) await room.webSocketMessage(client, JSON.stringify(fileDragPreview(change)));
  for (const change of [
    { x: -1 }, { x: 65536 }, { y: 0.5 }, { generation: 0 }, { generation: 0x1_0000_0000 },
    { viewportRevision: -1 }, { viewportRevision: 0x1_0000_0000 }, { viewportRevision: undefined }, { extra: 1 },
  ]) await room.webSocketMessage(client, JSON.stringify(fileDragPreview({ drop: { ...fileDragPreview().drop, ...change } })));
  assert.deepEqual(host.messages, []);
  assert.deepEqual(client.messages, []);
  assert.deepEqual(client.closed, []);
  assert.deepEqual(host.closed, []);
});

test("drag previews admit inclusive numeric/type limits and enforce the raw UTF8 ceiling", async () => {
  const { room, sockets: [client, host] } = createRoom();
  const value = fileDragPreview({ dragId: 0xffff_ffff, types: Array(64).fill("a".repeat(96)),
    drop: { x: 65535, y: 0, generation: 0xffff_ffff, viewportRevision: 0xffff_ffff } });
  const bounded = JSON.stringify(value).padEnd(8192, " ");
  assert.equal(Buffer.byteLength(bounded), 8192);
  await room.webSocketMessage(client, bounded);
  await room.webSocketMessage(client, bounded + " ");
  const escaped = JSON.stringify(value).replaceAll("a", "\\u0061");
  assert.ok(Buffer.byteLength(escaped) > 8192);
  await room.webSocketMessage(client, escaped);
  assert.deepEqual(host.messages, [bounded]);
});

test("drag previews never queue for a later host", async () => {
  const client = new FakeSocket("client");
  client.attachment.mediaEpoch = CURRENT_MEDIA_EPOCH;
  const writes = [];
  const storage = { get: async () => [], put: async (...args) => writes.push(args), delete: async () => {} };
  const room = new Room({ getWebSockets: () => [client], storage }, {});
  await room.webSocketMessage(client, JSON.stringify(fileDragPreview()));
  assert.deepEqual(writes, []);
});

test("relays follow controls only from the client with a boolean flag on the current v4 epoch", async () => {
  const { room, sockets: [client, host] } = createRoom();
  const on = JSON.stringify({ type: "media-control", action: "follow", follow: true, protocol: 4, epoch: CURRENT_MEDIA_EPOCH, reason: "user-follow" });
  const off = JSON.stringify({ type: "media-control", action: "follow", follow: false, protocol: 4, epoch: CURRENT_MEDIA_EPOCH, reason: "user-follow" });
  await room.webSocketMessage(client, on);
  await room.webSocketMessage(client, off);
  for (const invalid of [
    { ...JSON.parse(on), follow: "true" },
    { ...JSON.parse(on), follow: 1 },
    { type: "media-control", action: "follow", protocol: 4, epoch: CURRENT_MEDIA_EPOCH },
    { ...JSON.parse(on), epoch: STALE_MEDIA_EPOCH },
    { ...JSON.parse(on), protocol: 2 },
    { type: "media-control", action: "follow", follow: true, protocol: 4 },
  ]) await room.webSocketMessage(client, JSON.stringify(invalid));
  await room.webSocketMessage(host, on);
  assert.deepEqual(host.messages, [on, off]);
  assert.deepEqual(client.messages, []);
  assert.deepEqual(client.closed, []);
  assert.deepEqual(host.closed, []);
});

test("buffers only the latest follow control until the host connects", async () => {
  const client = new FakeSocket("client");
  client.attachment.mediaEpoch = CURRENT_MEDIA_EPOCH;
  let pending = [];
  const storage = {
    get: async () => pending,
    put: async (_key, value) => { pending = value; },
    delete: async () => { pending = []; },
  };
  const room = new Room({ getWebSockets: () => [client], storage }, { TOKEN: "test" });
  const follow = value => JSON.stringify({ type: "media-control", action: "follow", follow: value, protocol: 4, epoch: CURRENT_MEDIA_EPOCH, reason: "user-follow" });
  const refresh = JSON.stringify({ type: "media-control", action: "monitors", protocol: 4, epoch: CURRENT_MEDIA_EPOCH });
  await room.webSocketMessage(client, follow(true));
  await room.webSocketMessage(client, refresh);
  await room.webSocketMessage(client, follow(false));
  await room.webSocketMessage(client, JSON.stringify({ ...JSON.parse(follow(true)), follow: "yes" }));
  assert.deepEqual(pending, [refresh, follow(false)]);
});

test("relays the first-frame handshake in the correct direction and buffers only requests", async () => {
  const { room, sockets: [client, host] } = createRoom();
  const request = JSON.stringify({ type: "first-frame", protocol: 1, phase: "request", requestId: "firstFrame_1", transport: "webrtc" });
  const admitted = JSON.stringify({ type: "first-frame", protocol: 1, phase: "admitted", requestId: "firstFrame_1", generation: 4 });
  const failed = JSON.stringify({ type: "first-frame", protocol: 1, phase: "failed", requestId: "firstFrame_1", generation: 4, reason: "no-idr-admitted" });
  const presented = JSON.stringify({ type: "first-frame", protocol: 1, phase: "presented", requestId: "firstFrame_1", generation: 4 });
  await room.webSocketMessage(client, request);
  await room.webSocketMessage(host, admitted);
  await room.webSocketMessage(client, presented);
  await room.webSocketMessage(host, failed);
  assert.deepEqual(host.messages, [request, presented]);
  assert.deepEqual(client.messages, [admitted, failed]);
  await room.webSocketMessage(host, JSON.stringify({ type: "first-frame", protocol: 1, phase: "request", requestId: "firstFrame_2", transport: "webrtc" }));
  assert.deepEqual(client.messages, [admitted, failed]);
});

test("relays only a host-authenticated exact quality-keyframe admission", async () => {
  const { room, sockets: [client, host] } = createRoom();
  const admitted = JSON.stringify({
    type: "first-frame",
    protocol: 1,
    phase: "quality-keyframe-admitted",
    requestId: "quality-01234567-89ab-4def-8123-456789abcdef",
    transport: "wss",
    generation: 7,
    sequence: 463,
    codec: "hevc",
    reason: "stream-lab-quality-capture",
  });
  await room.webSocketMessage(host, admitted);
  for (const invalid of [
    { ...JSON.parse(admitted), sequence: -1 },
    { ...JSON.parse(admitted), requestId: "firstFrame_1" },
    { ...JSON.parse(admitted), transport: "webrtc" },
    { ...JSON.parse(admitted), reason: "diagnostic-injected-fault" },
  ]) await room.webSocketMessage(host, JSON.stringify(invalid));
  await room.webSocketMessage(client, admitted);
  assert.deepEqual(client.messages, [admitted]);
  assert.deepEqual(host.messages, []);
});

test("drops malformed or wrong-role first-frame phases without closing peers", async () => {
  const { room, sockets: [client, host] } = createRoom();
  await room.webSocketMessage(client, JSON.stringify({ type: "first-frame", protocol: 1, phase: "admitted", requestId: "firstFrame_1", generation: 1 }));
  await room.webSocketMessage(host, JSON.stringify({ type: "first-frame", protocol: 1, phase: "request", requestId: "firstFrame_1", transport: "webrtc" }));
  await room.webSocketMessage(client, JSON.stringify({ type: "first-frame", protocol: 2, phase: "request", requestId: "bad", transport: "webrtc" }));
  assert.deepEqual(client.messages, []);
  assert.deepEqual(host.messages, []);
  assert.deepEqual(client.closed, []);
  assert.deepEqual(host.closed, []);
});

test("signaling relay carries binary input and cursor feedback in both directions", async () => {
  const { room, sockets: [client, host] } = createRoom();
  const sync = Uint8Array.of(10, 7, 0).buffer;
  const cursor = Uint8Array.of(128, 7, 0, 0, 64, 0, 192, 3).buffer;
  const cursorType = Uint8Array.of(129, 1).buffer;
  await room.webSocketMessage(client, sync);
  await room.webSocketMessage(host, cursor);
  await room.webSocketMessage(host, cursorType);
  assert.deepEqual(host.messages, [sync]);
  assert.deepEqual(client.messages, [cursor, cursorType]);
});
