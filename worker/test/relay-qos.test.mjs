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
      wssTopology: { laneCount: 16, dataShards: 15, parityShards: 1 },
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
      wssTopology: { laneCount: 16, dataShards: 15, parityShards: 1 },
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
      wssTopology: { laneCount: 16, dataShards: 15, parityShards: 1 },
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
  client.attachment = { role: null, expectedRole: "client", stripe: null, epoch: null };
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
  client.attachment = { role: null, expectedRole: "client", stripe: null, epoch: null };
  const sockets = [host, client];
  const storage = { get: async () => [], put: async () => {}, delete: async () => {} };
  const room = new Room({ getWebSockets: () => sockets, storage }, { TOKEN: "test" });
  const capability = JSON.stringify({ type: "host-capabilities", serviceMode: false, coldBoot: false,
    secureDesktop: false, secureText: false, mediaProtocols: [2, 3], maxWssLanes: 16,
    wssFec: ["none", "xor-1"] });

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
      wssTopology: { laneCount: 16, dataShards: 15, parityShards: 1 },
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
  await room.webSocketMessage(host, JSON.stringify({ ...v2, effective: { ...v2.effective,
    wssTopology: { laneCount: 9, dataShards: 7, parityShards: 1 } } }));
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
      wssTopology: { laneCount: 16, dataShards: 15, parityShards: 1 },
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
      wssTopology: { laneCount: 16, dataShards: 15, parityShards: 1 },
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
      wssTopology: { laneCount: 16, dataShards: 15, parityShards: 1 },
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
      wssTopology: { laneCount: 16, dataShards: 15, parityShards: 1 },
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

test("relays only bounded protocol-v3 topology negotiation and dynamic ACKs", async () => {
  const { room, sockets: [client, host] } = createRoom();
  const topology = { laneCount: 5, dataShards: 4, parityShards: 1 };
  const duplicateParityTopology = { laneCount: 16, dataShards: 14, parityShards: 2 };
  const start = JSON.stringify({ type: "media-control", action: "start", protocol: 3,
    epoch: CURRENT_MEDIA_EPOCH, codec: "hevc", topology });
  const ack = JSON.stringify({ type: "media-ack-v3", protocol: 3, epoch: CURRENT_MEDIA_EPOCH,
    generation: 7, topology, lanes: [10, 11, 12, 13, 14], reconstructed: 14, presented: 13,
    recoveredFrames: 1, expiredFrames: 0, decoderDrops: 0, dependencyLosses: 0,
    deadlineMs: 220, reassemblyP95Ms: 4, recoveryState: "healthy",
    completionMissingByLane: [1, 7, 0, 2, 14] });
  const accepted = JSON.stringify({ type: "media-topology", protocol: 3, epoch: CURRENT_MEDIA_EPOCH,
    status: "accepted", topology });
  await room.webSocketMessage(client, start);
  await room.webSocketMessage(client, ack);
  await room.webSocketMessage(host, accepted);
  const duplicateParityStart = JSON.stringify({ type: "media-control", action: "start", protocol: 3,
    epoch: CURRENT_MEDIA_EPOCH, codec: "hevc", topology: duplicateParityTopology });
  const duplicateParityAccepted = JSON.stringify({ type: "media-topology", protocol: 3,
    epoch: CURRENT_MEDIA_EPOCH, status: "accepted", topology: duplicateParityTopology });
  await room.webSocketMessage(client, duplicateParityStart);
  await room.webSocketMessage(host, duplicateParityAccepted);

  for (const invalid of [
    { type: "media-control", action: "start", protocol: 3, epoch: CURRENT_MEDIA_EPOCH,
      topology: { laneCount: 17, dataShards: 16, parityShards: 1 } },
    { type: "media-control", action: "start", protocol: 3, epoch: CURRENT_MEDIA_EPOCH,
      topology: { laneCount: 5, dataShards: 3, parityShards: 1 } },
    { type: "media-control", action: "start", protocol: 3, epoch: STALE_MEDIA_EPOCH, topology },
    { type: "media-ack-v3", protocol: 3, epoch: CURRENT_MEDIA_EPOCH, generation: 7,
      topology, lanes: [1, 2, 3, 4] },
    { type: "media-ack-v3", protocol: 3, epoch: CURRENT_MEDIA_EPOCH, generation: 7,
      topology, lanes: [1, 2, 3, 4, 5], reconstructed: 5, presented: 5,
      recoveredFrames: 0, expiredFrames: 0, decoderDrops: 0, dependencyLosses: 0,
      deadlineMs: 220, reassemblyP95Ms: 4, recoveryState: "healthy",
      completionMissingByLane: [1, 2, 3, 4] },
  ]) await room.webSocketMessage(client, JSON.stringify(invalid));
  await room.webSocketMessage(host, JSON.stringify({ type: "media-topology", protocol: 3,
    epoch: CURRENT_MEDIA_EPOCH, status: "accepted", topology: { laneCount: 5, dataShards: 3, parityShards: 1 } }));

  assert.deepEqual(host.messages, [start, ack, duplicateParityStart]);
  assert.deepEqual(client.messages, [accepted, duplicateParityAccepted]);
});

test("relays the Rust host's accepted topology wire with a null optional reason", async () => {
  const { room, sockets: [client, host] } = createRoom();
  const topology = { laneCount: 16, dataShards: 15, parityShards: 1 };
  const accepted = JSON.stringify({
    type: "media-topology",
    protocol: 3,
    epoch: CURRENT_MEDIA_EPOCH,
    status: "accepted",
    topology,
    reason: null,
  });

  await room.webSocketMessage(host, accepted);

  assert.deepEqual(client.messages, [accepted]);
});

test("host capabilities bound advertised WSS protocol and lane support", async () => {
  const { room, sockets: [client, host] } = createRoom();
  const capability = JSON.stringify({ type: "host-capabilities", serviceMode: true, coldBoot: true,
    secureDesktop: true, secureText: true, mediaProtocols: [2, 3], maxWssLanes: 16,
    wssFec: ["none", "xor-1", "rs-2"] });
  await room.webSocketMessage(host, capability);
  await room.webSocketMessage(host, JSON.stringify({ type: "host-capabilities", serviceMode: true,
    coldBoot: true, secureDesktop: true, secureText: true, mediaProtocols: [2, 4], maxWssLanes: 16,
    wssFec: ["none", "xor-1"] }));
  assert.deepEqual(client.messages, [capability]);
});

test("relays monitor lists and bounded monitor selections", async () => {
  const { room, sockets: [client, host] } = createRoom();
  const list = JSON.stringify({ type: "monitor-list", monitors: [{ selector: "\\\\.\\\\DISPLAY1", name: "Primary", width: 1920, height: 1080, selected: true }] });
  const select = JSON.stringify({ type: "media-control", action: "monitor", protocol: 2, monitor: "\\\\.\\\\DISPLAY2" });
  await room.webSocketMessage(host, list);
  await room.webSocketMessage(client, select);
  await room.webSocketMessage(client, JSON.stringify({ type: "media-control", action: "monitor", monitor: "x".repeat(129) }));
  assert.deepEqual(client.messages, [list]);
  assert.deepEqual(host.messages, [select]);
  assert.deepEqual(client.closed, []);
  assert.deepEqual(host.closed, []);
});

test("relays only v2 epoch-scoped idempotent monitor refresh requests", async () => {
  const { room, sockets: [client, host] } = createRoom();
  const epoch = CURRENT_MEDIA_EPOCH;
  const refresh = JSON.stringify({ type: "media-control", action: "monitors", protocol: 2, epoch });
  const replay = JSON.stringify({ type: "media-control", action: "monitors", protocol: 2, epoch: STALE_MEDIA_EPOCH });
  await room.webSocketMessage(client, refresh);
  await room.webSocketMessage(client, replay);
  await room.webSocketMessage(client, JSON.stringify({ type: "media-control", action: "monitors", protocol: 1, epoch }));
  await room.webSocketMessage(client, JSON.stringify({ type: "media-control", action: "monitors", protocol: 2 }));
  await room.webSocketMessage(client, JSON.stringify({ type: "media-control", action: "monitors", protocol: 2, epoch: "A".repeat(32) }));
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
  const refresh = JSON.stringify({ type: "media-control", action: "monitors", protocol: 2, epoch: CURRENT_MEDIA_EPOCH });
  const replay = JSON.stringify({ type: "media-control", action: "monitors", protocol: 2, epoch: STALE_MEDIA_EPOCH });

  await room.webSocketMessage(client, refresh);
  await room.webSocketMessage(client, replay);

  assert.deepEqual(pending, [refresh]);
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
  await room.webSocketMessage(client, sync);
  await room.webSocketMessage(host, cursor);
  assert.deepEqual(host.messages, [sync]);
  assert.deepEqual(client.messages, [cursor]);
});
