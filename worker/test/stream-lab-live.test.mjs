import { readClientSource } from "./helpers/client-source.mjs";
import test from "node:test";
import assert from "node:assert/strict";
import { EventEmitter } from "node:events";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import * as liveModule from "../tools/stream-lab-live.mjs";

import {
  benchmarkProfile,
  benchStatusConfirmsApply,
  aggressiveOptimizationContract,
  applyWithFirstFrameRetry,
  assertReceiverAssetBuild,
  assertReceiverCompatibility,
  assertWorkerCompatibility,
  installReceiverAssetOverrides,
  aggregateExperimentRepetitions,
  buildHybridWorkloadPlan,
  buildTemporalCapturePlan,
  buildReceiverUrl,
  buildLabHostUrl,
  buildCandidateHostLaunch,
  buildReceiverChromeArgs,
  buildProductionProbeUrl,
  buildTopologyTrialPlan,
  buildCandidateProfileLease,
  captureReceiverFrame,
  candidateStartupRejection,
  classifyCandidateProfileFailure,
  classifyMonitorSelectionFailure,
  chooseOptimizationFinalists,
  collectLaneRecoveryQualitySequence,
  collectQualityRepetitions,
  combinePerformanceAndQuality,
  defaultTopologyFrontier,
  exactFrameAlignment,
  performanceFrameTrace,
  firstFrameReadiness,
  hevcTileDecoderAdmission,
  hybridSceneRequirement,
  nativeFrameState,
  NATIVE_RENDERED_AGE_LIMIT_MS,
  shouldRetryMeasuredFrame,
  effectiveEncoderBitrateMbps,
  evaluateLaneRecoveryQuality,
  evaluateWssLaneRecovery,
  evaluateOptimizationRun,
  initialEncoderTargetMbps,
  installTerminationCleanup,
  interruptedCampaignState,
  laneRecoveryTopology,
  measureOptimizationScreenCandidate,
  inspectCleanupResources,
  nextStabilizedBitrateMbps,
  normalizeDecoderPath,
  normalizeReceiverOptions,
  sameReceiverOptions,
  qualityAnchorAlignmentOptions,
  runFreshCheckpoint,
  LiveStreamLabSession,
  runCheckpointedUnit,
  retainWorstSamplePairs,
  placeSourceWindow,
  prepareQualityDeltaFrame,
  prepareQualitySourceFrame,
  qualitySampleStem,
  SerializedLeaseHeartbeat,
  selectHostMonitor,
  selectPersistentVirtualDisplay,
  summarizeAlignmentFailure,
  virtualMonitorSelectionCurrent,
  shouldRetryQualityFrameMatch,
  shouldRecoverRejectedCandidate,
  selectTransientDisplay,
  windowCoversDisplay,
  shutdownChrome,
  shapingFailureReasons,
  stabilizeEncoderForCap,
  summarizePerformance,
  summarizeProductionProbe,
} from "../tools/stream-lab-live.mjs";
import { STREAM_LAB_COMPATIBILITY } from "../public/media.js";

test("quiescent hybrid treats animated chrome canvas as a motion guard", () => {
  assert.equal(hybridSceneRequirement("png-quiescent-detail", "chrome-canvas"), "guard");
  assert.equal(hybridSceneRequirement("png-quiescent-detail", "chrome-text"), "tile");
});

test("no-action mode suppresses quiescent cursor and viewport interaction trials", () => {
  assert.equal(liveModule.shouldRunQuiescentInteractionTrials("png-quiescent-detail", false), false);
  assert.equal(liveModule.shouldRunQuiescentInteractionTrials("png-quiescent-detail", true), true);
  assert.equal(liveModule.shouldRunQuiescentInteractionTrials("off", true), false);
});

test("HEVC tile admission classifies an unsupported browser before quality waits", () => {
  assert.deepEqual(hevcTileDecoderAdmission({
    wssMedia: { hybrid: {
      decoderCapability: "unsupported",
      decoderCapabilityCodec: "hev1.1.6.L120.B0",
    } },
  }, { hybridMode: "hevc-quiescent-tiles" }), {
    required: true,
    ready: false,
    terminal: true,
    state: "unsupported",
    codec: "hev1.1.6.L120.B0",
  });
  assert.deepEqual(hevcTileDecoderAdmission({
    wssMedia: { hybrid: { decoderCapability: "supported" } },
  }, { hybridMode: "hevc-quiescent-tiles" }), {
    required: true,
    ready: true,
    terminal: false,
    state: "supported",
    codec: null,
  });
  assert.deepEqual(hevcTileDecoderAdmission({}, { hybridMode: "off" }), {
    required: false,
    ready: true,
    terminal: false,
    state: null,
    codec: null,
  });
});

test("production probe stays off lab routes and identifies a recovered presentation stall", () => {
  assert.equal(
    buildProductionProbeUrl("https://play.example/special?diagnostics=1&labRun=old#state").href,
    "https://play.example/?diag=1",
  );
  const summary = summarizeProductionProbe([
    { atMs: 0, presented: 10, reconstructed: 10, receivedPackets: 100, signalingRegistered: true },
    { atMs: 250, presented: 11, reconstructed: 11, receivedPackets: 116, signalingRegistered: true },
    { atMs: 500, presented: 12, reconstructed: 12, receivedPackets: 132, signalingRegistered: true },
    { atMs: 750, presented: 12, reconstructed: 12, receivedPackets: 148, signalingRegistered: true },
    { atMs: 1_250, presented: 12, reconstructed: 12, receivedPackets: 180, signalingRegistered: true },
    { atMs: 1_750, presented: 12, reconstructed: 12, receivedPackets: 212, signalingRegistered: true },
    {
      atMs: 2_250,
      presented: 13,
      reconstructed: 13,
      receivedPackets: 244,
      signalingRegistered: true,
      hostCapabilitiesCurrent: true,
      hostCapabilities: { mediaProtocols: [2, 3], maxWssLanes: 16, wssFec: ["none", "xor-1"] },
      protocol: 3,
      topology: { laneCount: 16, dataShards: 15, parityShards: 1 },
      topologyAccepted: true,
      openLanes: 16,
      highestSequenceByLane: Array(16).fill(13),
    },
  ], { stallThresholdMs: 750 });
  assert.equal(summary.result, "complete");
  assert.equal(summary.admission.admitted, true);
  assert.equal(summary.inputAutomation, false);
  assert.equal(summary.candidateHostStarted, false);
  assert.equal(summary.presentationStalls.length, 1);
  assert.deepEqual(summary.presentationStalls[0], {
    startedAtMs: 500,
    endedAtMs: 2_250,
    durationMs: 1_750,
    receivedPacketsDelta: 112,
    recovered: true,
  });
});

test("production probe can exercise real public assets with a lab-scoped topology", () => {
  const labRun = "123e4567-e89b-42d3-a456-426614174000";
  const topology = { laneCount: 8, dataShards: 7, parityShards: 1 };
  assert.equal(
    buildProductionProbeUrl("https://play.example/special?old=1#state", { labRun, wssTopology: topology }).href,
    `https://play.example/?diag=1&labRun=${labRun}&wssLanes=8&wssParity=1`,
  );

  const admitted = summarizeProductionProbe([{
    atMs: 500,
    presented: 1,
    reconstructed: 1,
    receivedPackets: 8,
    signalingRegistered: true,
    hostCapabilitiesCurrent: true,
    hostCapabilities: { mediaProtocols: [2, 3], maxWssLanes: 16, wssFec: ["none", "xor-1"] },
    protocol: 3,
    topology,
    topologyAccepted: true,
    openLanes: 8,
    highestSequenceByLane: Array(8).fill(1),
  }], { wssTopology: topology });
  assert.equal(admitted.result, "complete");
  assert.deepEqual(admitted.admission.requiredTopology, topology);
});

test("production probe fails admission when signaling has no host or media evidence", () => {
  const summary = summarizeProductionProbe([
    {
      atMs: 0,
      presented: 0,
      reconstructed: 0,
      receivedPackets: 0,
      signalingRegistered: true,
      hostCapabilitiesCurrent: false,
      hostCapabilities: null,
      protocol: 2,
      topology: { laneCount: 4, dataShards: 3, parityShards: 1 },
      topologyAccepted: true,
      openLanes: 4,
      highestSequenceByLane: [0, 0, 0, 0],
    },
  ]);
  assert.equal(summary.result, "fail");
  assert.equal(summary.admission.admitted, false);
  assert.equal(summary.error.code, "production-probe-admission-failed");
  assert.deepEqual(summary.admission.reasons, [
    "host-capabilities-not-current",
    "host-capability-mismatch",
    "topology-mismatch",
    "topology-not-accepted",
    "lanes-not-open",
    "lane-sequences-missing",
    "no-media-packets",
    "no-reconstructed-frames",
    "no-presented-frames",
  ]);
});

test("Stream Lab rejects stale Worker and receiver compatibility before host launch", () => {
  assert.deepEqual(assertWorkerCompatibility({
    ok: true,
    compatibility: STREAM_LAB_COMPATIBILITY,
  }), STREAM_LAB_COMPATIBILITY);
  assert.throws(() => assertWorkerCompatibility({
    ok: true,
    compatibility: { ...STREAM_LAB_COMPATIBILITY, hostTrace: { version: 1, bytes: 12 } },
  }), error => error?.code === "worker-compatibility-mismatch");

  const snapshot = {
    assetBuild: {
      index: STREAM_LAB_COMPATIBILITY.receiverBuild,
      media: STREAM_LAB_COMPATIBILITY.receiverBuild,
    },
    compatibility: STREAM_LAB_COMPATIBILITY,
  };
  assert.deepEqual(assertReceiverCompatibility(snapshot), {
    assetBuild: snapshot.assetBuild,
    compatibility: STREAM_LAB_COMPATIBILITY,
  });
  assert.throws(() => assertReceiverCompatibility({
    ...snapshot,
    compatibility: { ...STREAM_LAB_COMPATIBILITY, benchStatusProtocol: 1 },
  }), error => error?.code === "receiver-compatibility-mismatch");
});

test("candidate profile failures prefer compatibility and packet evidence over a generic timeout", () => {
  assert.equal(classifyCandidateProfileFailure({
    signaling: { benchStatusRejection: { reason: "hybrid-activity-required" } },
  }), "bench-status-rejected");
  assert.equal(classifyCandidateProfileFailure({
    wssMedia: { packetRejects: { total: 16 } },
  }), "media-packet-rejected");
  assert.equal(classifyCandidateProfileFailure({
    wssMedia: { receivedPackets: 16, reconstructed: 0 },
  }), "wss-startup-no-reconstruction");
  assert.equal(classifyCandidateProfileFailure({
    wssMedia: { receivedPackets: 0, reconstructed: 0 },
  }), "candidate-profile-timeout");
});

test("quiescent quality anchors bind the acknowledged clean IDR before tile warm-up", () => {
  assert.deepEqual(
    qualityAnchorAlignmentOptions("png-quiescent-detail", "ui-text", 230),
    { presentedNetworkSequence: 230 },
  );
  assert.deepEqual(
    qualityAnchorAlignmentOptions("png-quiescent-detail", "motion-60", 230),
    { presentedNetworkSequence: 230 },
  );
  assert.deepEqual(
    qualityAnchorAlignmentOptions("png-static-roi", "ui-text", 230),
    { exactNetworkSequence: 230 },
  );
  assert.deepEqual(
    qualityAnchorAlignmentOptions("png-quiescent-detail", "source-code", 0xffff_ffff),
    { presentedNetworkSequence: 0xffff_ffff },
  );
});

test("delta quality waits until every WSS lane has delivered the acknowledged IDR anchor", () => {
  assert.equal(typeof liveModule.exactQualityAnchorAlignment, "function");
  const source = {
    generation: 7,
    scene: "thin-lines",
    seed: 469,
    frameId: 0,
    epochMs: 1_788_284_549_442,
  };
  const diagnostics = {
    route: "wss",
    glassToGlass: {
      sequence: 0,
      timestamp: source.epochMs >>> 0,
    },
    frameTrace: {
      sequence: 802,
      key: true,
      encodedBytes: 13_506,
      recovered: true,
    },
    wssMedia: {
      protocol: 3,
      topology: { laneCount: 16, dataShards: 15, parityShards: 1 },
      openLanes: 16,
      highestSequenceByLane: Array(16).fill(802),
    },
  };
  diagnostics.wssMedia.highestSequenceByLane[14] = 799;

  assert.throws(() => liveModule.exactQualityAnchorAlignment(
    source,
    diagnostics,
    "quality",
    qualityAnchorAlignmentOptions("png-quiescent-detail", source.scene, 802),
    802,
  ), error => error?.code === "quality-anchor-lane-delivery-pending" &&
    error?.detail?.deliveredLanes === 15 &&
    error?.detail?.laggingLanes?.[0]?.lane === 14 &&
    error?.detail?.laggingLanes?.[0]?.sequence === 799);

  diagnostics.wssMedia.highestSequenceByLane[14] = 802;
  const aligned = liveModule.exactQualityAnchorAlignment(
    source,
    diagnostics,
    "quality",
    qualityAnchorAlignmentOptions("png-quiescent-detail", source.scene, 802),
    802,
  );
  assert.equal(aligned.trace.network.sequence, 802);
  assert.equal(aligned.laneDelivery.deliveredLanes, 16);
  assert.deepEqual(aligned.laneDelivery.laggingLanes, []);
});

test("quiescent interaction readiness rejects tiles from the previous source frame", () => {
  assert.equal(typeof liveModule.quiescentInteractionReady, "function");
  const baseline = {
    receivedUpserts: 12, composites: 465,
  };
  const source = {
    scene: "source-code", frameId: 12, epochMs: 1_306_142_345,
  };
  const stale = {
    glassToGlass: { sequence: 4, timestamp: 1_306_144_442 },
    wssMedia: { hybrid: {
      activeTiles: 4, readyTiles: 4, pendingTiles: 0,
      receivedUpserts: 12, composites: 465, staleComposites: 0,
    } },
  };
  const settled = {
    glassToGlass: { sequence: 12, timestamp: source.epochMs >>> 0 },
    wssMedia: { hybrid: {
      activeTiles: 4, readyTiles: 4, pendingTiles: 0,
      receivedUpserts: 16, composites: 469, staleComposites: 0,
    } },
  };

  assert.equal(liveModule.quiescentInteractionReady(stale, source, baseline), false);
  assert.equal(liveModule.quiescentInteractionReady(settled, source, baseline), true);
});

test("quiescent transitions wait for the committed viewport and matching tile generation", () => {
  assert.equal(typeof liveModule.quiescentTransitionCommitted, "function");
  const before = {
    viewport: { revision: 1 },
    wssMedia: { hybrid: { composites: 40 } },
  };
  const refining = {
    mediaGeneration: 4,
    viewport: { revision: 2, generation: 0, pending: 2, state: "refining" },
    wssMedia: { hybrid: {
      generation: 3, cropRevision: 1, activeTiles: 4, readyTiles: 4, pendingTiles: 0,
      composites: 44,
    } },
  };
  const committed = {
    mediaGeneration: 4,
    viewport: { revision: 2, generation: 4, pending: null, state: "sharp 1.7x/1.7x" },
    wssMedia: { hybrid: {
      generation: 4, cropRevision: 2, activeTiles: 2, readyTiles: 2, pendingTiles: 0,
      composites: 45,
    } },
  };

  assert.equal(liveModule.quiescentTransitionCommitted(before, refining), false);
  assert.equal(liveModule.quiescentTransitionCommitted(before, committed), true);
});

test("cursor neutrality waits for every input packet and a newer host telemetry window", () => {
  assert.equal(typeof liveModule.quiescentCursorSweepSettled, "function");
  assert.equal(typeof liveModule.quiescentCursorCounterChanges, "function");
  const before = {
    framesDrawn: 100,
    motionSent: 12,
    benchStatus: { runId: "run-1", sequence: 7 },
    wssMedia: { hybrid: {
      receivedInvalidations: 3,
      receivedUpserts: 9,
      pngBytes: 120_000,
    } },
  };
  const onlyPresented = {
    ...before,
    framesDrawn: 104,
  };
  const onlyNewWindow = {
    ...before,
    benchStatus: { runId: "run-1", sequence: 8 },
  };
  const settled = {
    ...onlyNewWindow,
    motionSent: 16,
  };
  const damaged = {
    ...settled,
    wssMedia: { hybrid: {
      ...settled.wssMedia.hybrid,
      receivedInvalidations: 4,
      pngBytes: 121_024,
    } },
  };

  assert.equal(liveModule.quiescentCursorSweepSettled(before, onlyPresented), false);
  assert.equal(liveModule.quiescentCursorSweepSettled(before, onlyNewWindow), false);
  assert.equal(liveModule.quiescentCursorSweepSettled(before, settled, 4), true);
  assert.deepEqual(liveModule.quiescentCursorCounterChanges(before, settled), []);
  assert.deepEqual(liveModule.quiescentCursorCounterChanges(before, damaged),
    ["receivedInvalidations", "pngBytes"]);
});

test("quality capture atomically paints the paused source identity before crossing the barrier", async () => {
  const calls = [];
  const metadata = { scene: "gradients", frameId: 0, paused: true, generation: 8, epochMs: 1234 };
  const source = {
    async evaluate(script) {
      calls.push(script);
      if (script.includes("requestAnimationFrame")) return metadata;
      if (script.includes("metadata()")) return metadata;
      return true;
    },
  };
  const result = await prepareQualitySourceFrame(source, "gradients", 0, {
    poller: async (probe) => probe(),
  });
  assert.equal(result, metadata);
  assert.match(calls[0], /setPausedScene\("gradients",469,0\)/);
  assert.doesNotMatch(calls[0], /\.setScene\(/);
  assert.doesNotMatch(calls[0], /\.pauseAt\(/);
  assert.match(calls.at(-1), /requestAnimationFrame[\s\S]*requestAnimationFrame/);
});

test("quiescent interaction anchors can request and bind one clean quality keyframe", async () => {
  assert.equal(typeof liveModule.requestCleanQualityKeyframe, "function");
  const requestId = "quality-01234567-89ab-cdef-0123-456789abcdef";
  const calls = [];
  const receiver = {
    async evaluate(script) {
      calls.push(script);
      if (script.includes("injectDecoderFault")) return true;
      if (script.includes("qualityKeyframeAck")) return { sequence: 417, generation: 2 };
      return null;
    },
  };

  const ack = await liveModule.requestCleanQualityKeyframe(receiver, requestId, {
    label: "source-code:0",
    poller: async probe => probe(),
  });
  assert.equal(ack.sequence, 417);
  assert.match(calls[0], /stream-lab-quality-capture/);
  assert.match(calls[0], /quality-01234567-89ab-cdef-0123-456789abcdef/);
  assert.match(calls[1], /qualityKeyframeAck/);
});

test("delta proof arms a bounded receiver identity-history window", async () => {
  const calls = [];
  const receiver = {
    async evaluate(script) {
      calls.push(script);
      return true;
    },
  };

  assert.equal(await liveModule.armPresentedIdentityHistory(receiver, 16), true);
  assert.match(calls[0], /armPresentedIdentityHistory\?\.\(16\)/);
  assert.throws(() => liveModule.armPresentedIdentityHistory(receiver, 0), /between 1 and 64/);
  assert.throws(() => liveModule.armPresentedIdentityHistory(receiver, 65), /between 1 and 64/);
});

test("delta quality capture advances exactly one painted frame after the clean anchor", async () => {
  const calls = [];
  const anchor = { scene: "localized-change", frameId: 60, paused: true, generation: 8, epochMs: 1234 };
  const delta = { ...anchor, frameId: 61, epochMs: 1267 };
  const source = {
    async evaluate(script) {
      calls.push(script);
      if (script.includes("requestAnimationFrame")) return delta;
      if (script.includes("metadata()")) return delta;
      return true;
    },
  };
  const result = await prepareQualityDeltaFrame(source, anchor, {
    poller: async (probe) => probe(),
  });
  assert.equal(result, delta);
  assert.match(calls[0], /pauseAt\(61\)/);
  assert.match(calls.at(-1), /requestAnimationFrame[\s\S]*requestAnimationFrame/);
});

test("delta quality capture can wait for one encoded trigger before advancing to the scored frame", async () => {
  const calls = [];
  let current = { scene: "localized-change", frameId: 59, paused: true, generation: 8, epochMs: 1234 };
  const source = {
    async evaluate(script) {
      calls.push(script);
      const target = /pauseAt\((\d+)\)/.exec(script)?.[1];
      if (target !== undefined) {
        current = { ...current, frameId: Number(target), epochMs: current.epochMs + 33 };
        return true;
      }
      return current;
    },
  };
  const waited = [];
  const result = await prepareQualityDeltaFrame(source, current, {
    poller: async (probe) => probe(),
    waitForTrigger: async (metadata) => {
      waited.push(metadata.frameId);
      assert.equal(calls.filter(script => script.includes("pauseAt")).length, 1);
      return true;
    },
  });
  assert.equal(result.frameId, 61);
  assert.deepEqual(waited, [60]);
  assert.deepEqual(
    calls.filter(script => script.includes("pauseAt")).map(script => Number(/pauseAt\((\d+)\)/.exec(script)[1])),
    [60, 61],
  );
});

test("hybrid delta capture warms three aligned frames before scoring the tile-ready frame", async () => {
  let current = { scene: "source-code", frameId: 10, paused: true, generation: 8, epochMs: 1234 };
  const source = {
    async evaluate(script) {
      const target = /pauseAt\((\d+)\)/.exec(script)?.[1];
      if (target !== undefined) {
        current = { ...current, frameId: Number(target), epochMs: current.epochMs + 33 };
        return true;
      }
      return current;
    },
  };
  const waited = [];
  const result = await prepareQualityDeltaFrame(source, current, {
    poller: async (probe) => probe(),
    triggerFrames: 3,
    waitForTrigger: async metadata => (waited.push(metadata.frameId), true),
  });
  assert.equal(result.frameId, 14);
  assert.deepEqual(waited, [11, 12, 13]);
});

test("feedback delta capture warms nine aligned frames before scoring the first eligible frame", async () => {
  let current = { scene: "ui-text", frameId: 0, paused: true, generation: 8, epochMs: 1234 };
  const source = {
    async evaluate(script) {
      const target = /pauseAt\((\d+)\)/.exec(script)?.[1];
      if (target !== undefined) {
        current = { ...current, frameId: Number(target), epochMs: current.epochMs + 33 };
        return true;
      }
      return current;
    },
  };
  const waited = [];
  const result = await prepareQualityDeltaFrame(source, current, {
    poller: async (probe) => probe(),
    triggerFrames: 9,
    waitForTrigger: async metadata => (waited.push(metadata.frameId), true),
  });
  assert.equal(result.frameId, 10);
  assert.deepEqual(waited, [1, 2, 3, 4, 5, 6, 7, 8, 9]);
});

test("paired delta quality resolves the same scored frame for control and hybrid candidates", () => {
  assert.equal(liveModule.qualityDeltaTriggerFrames({ requireHybridActivity: false }), 3);
  assert.equal(liveModule.qualityDeltaTriggerFrames({ requireHybridActivity: true }), 3);
  assert.equal(liveModule.qualityDeltaTriggerFrames({
    requireHybridActivity: false,
    deltaTriggerFrames: 1,
  }), 1);
});

test("cache-policy quality gates allow safe base-only control frames but require candidate tiles", () => {
  assert.equal(liveModule.hybridSceneRequirement(
    "png-quiescent-detail", "ui-text", "dda-gated"), "safe");
  assert.equal(liveModule.hybridSceneRequirement(
    "png-quiescent-detail", "ui-text", "suppress-reactivate"), "tile");
  assert.equal(liveModule.hybridSceneRequirement(
    "png-quiescent-detail", "motion-60", "dda-gated"), "guard");
});

test("delta quality artifacts use the exact scored frame rather than the IDR anchor", () => {
  assert.equal(
    qualitySampleStem("localized-change", { frameId: 61 }),
    "localized-change-61",
  );
});

test("quality timeout diagnostics retain the last exact-alignment rejection", () => {
  const failure = summarizeAlignmentFailure(Object.assign(
    new Error("The exact quality frame did not receive the intended QP map."),
    {
      code: "quality-roi-map-missing",
      detail: { sequence: 418, reason: "idr", changedCtbs: 0, totalCtbs: 2040 },
    },
  ));
  assert.deepEqual(failure, {
    code: "quality-roi-map-missing",
    message: "The exact quality frame did not receive the intended QP map.",
    detail: { sequence: 418, reason: "idr", changedCtbs: 0, totalCtbs: 2040 },
  });
});

test("quality capture retries one healthy stale static frame after the paint barrier", () => {
  const healthy = {
    route: "wss",
    signaling: { registered: true },
    wssMedia: {
      openLanes: 6,
      topology: { laneCount: 6, dataShards: 5, parityShards: 1 },
      receivedBytes: 1_000,
      dependencyLosses: 0,
      recoveryState: "healthy",
    },
  };
  assert.equal(shouldRetryQualityFrameMatch(healthy, 749, 0), false);
  assert.equal(shouldRetryQualityFrameMatch(healthy, 750, 0), true);
  assert.equal(shouldRetryQualityFrameMatch(healthy, 2_000, 1), false);
  assert.equal(shouldRetryQualityFrameMatch({
    ...healthy,
    wssMedia: { ...healthy.wssMedia, openLanes: 4 },
  }, 750, 0), false);
  assert.equal(shouldRetryQualityFrameMatch({
    ...healthy,
    wssMedia: { ...healthy.wssMedia, dependencyLosses: 1 },
  }, 750, 0), true);
  assert.equal(shouldRetryQualityFrameMatch({
    ...healthy,
    wssMedia: { ...healthy.wssMedia, dependencyLosses: 1, recoveryState: "awaiting-keyframe" },
  }, 750, 0), false);
});

test("quality capture writes pixels and frame identity from one atomic receiver snapshot", async () => {
  const directory = mkdtempSync(join(tmpdir(), "bubbleplay-frame-capture-"));
  const path = join(directory, "frame.png");
  const diagnostics = {
    glassToGlass: { sequence: 17, timestamp: 23, ageMs: 45 },
    frameTrace: { sequence: 91, encodedBytes: 24_918, key: true, recovered: false },
  };
  const receiver = {
    async evaluate(script) {
      assert.match(script, /captureFrame/);
      assert.match(script, /snapshot/);
      return {
        pngDataUrl: `data:image/png;base64,${Buffer.from("atomic-png").toString("base64")}`,
        diagnostics,
      };
    },
  };
  try {
    assert.deepEqual(await captureReceiverFrame(receiver, path), diagnostics);
    assert.equal(readFileSync(path, "utf8"), "atomic-png");
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("quality capture retries an atomic same-identity frame until its detail tile is drawable", async () => {
  const directory = mkdtempSync(join(tmpdir(), "bubbleplay-frame-capture-retry-"));
  const path = join(directory, "frame.png");
  const source = { scene: "gradients", seed: 469, generation: 8, frameId: 17, epochMs: 23 };
  const captures = [
    {
      pngDataUrl: `data:image/png;base64,${Buffer.from("base-only").toString("base64")}`,
      diagnostics: {
        glassToGlass: { sequence: 17, timestamp: 23, ageMs: 45 },
        frameTrace: {
          sequence: 91, encodedBytes: 4_000, key: false, recovered: false,
          hybridCropRevision: 0, hybridTilesDrawn: 0, hybridTileSuppressions: 4,
        },
      },
    },
    {
      pngDataUrl: `data:image/png;base64,${Buffer.from("enhanced").toString("base64")}`,
      diagnostics: {
        glassToGlass: { sequence: 17, timestamp: 23, ageMs: 48 },
        frameTrace: {
          sequence: 92, encodedBytes: 4_100, key: false, recovered: false,
          hybridCropRevision: 0, hybridTilesDrawn: 3, hybridTileReactivations: 3,
        },
      },
    },
  ];
  let calls = 0;
  const receiver = {
    async evaluate() {
      const capture = captures[Math.min(calls, captures.length - 1)];
      calls += 1;
      return capture;
    },
  };
  try {
    const diagnostics = await captureReceiverFrame(receiver, path, {
      source,
      phase: "quality-capture",
      alignmentOptions: { afterNetworkSequence: 90, requireDeltaFrame: true, hybridRequirement: "tile" },
      timeoutMs: 500,
    });
    assert.equal(calls, 2);
    assert.equal(diagnostics.frameTrace.sequence, 92);
    assert.equal(readFileSync(path, "utf8"), "enhanced");
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("lane recovery quality is preservation-gated with source identity mandatory", () => {
  const control = {
    vmafP5: 88, ssimP5: 0.94, worstTileSsim: 0.81, edgeFidelity: 0.72,
    colorError: 2, ocrCer: 0.08, blockArtifactSeverity: 4, exactFrameRatio: 1,
  };
  assert.equal(evaluateLaneRecoveryQuality(control, {
    ...control, vmafP5: 87.5, worstTileSsim: 0.805,
  }).accepted, true);
  const regressed = evaluateLaneRecoveryQuality(control, {
    ...control, vmafP5: 84, exactFrameRatio: 0.99,
  });
  assert.equal(regressed.accepted, false);
  assert.deepEqual(regressed.reasons.sort(), ["exact-frame-regression", "vmafP5-regression"]);
});

test("lane recovery accepts any validated parity-protected WSS v3 topology", () => {
  assert.deepEqual(laneRecoveryTopology({ laneCount: 6, dataShards: 5, parityShards: 1 }),
    { laneCount: 6, dataShards: 5, parityShards: 1 });
  assert.deepEqual(laneRecoveryTopology(),
    { laneCount: 16, dataShards: 15, parityShards: 1 });
  assert.throws(() => laneRecoveryTopology({ laneCount: 4, dataShards: 4, parityShards: 0 }),
    /parity-protected/);
  assert.throws(() => laneRecoveryTopology({ laneCount: 17, dataShards: 16, parityShards: 1 }),
    /parity-protected/);
});

test("lane recovery quality repetitions use unique artifacts and robust fixed-control aggregates", async () => {
  const calls = [];
  const captures = [
    { vmafP5: 54, ssimP5: 0.92, worstTileSsim: 0.41, edgeFidelity: 0.61,
      colorError: 4.8, ocrCer: 0.12, blockArtifactSeverity: 3.8, exactFrameRatio: 1 },
    { vmafP5: 47, ssimP5: 0.90, worstTileSsim: 0.38, edgeFidelity: 0.52,
      colorError: 5.7, ocrCer: 0.21, blockArtifactSeverity: 3.9, exactFrameRatio: 1 },
    { vmafP5: 53, ssimP5: 0.91, worstTileSsim: 0.40, edgeFidelity: 0.58,
      colorError: 5.0, ocrCer: 0.14, blockArtifactSeverity: 3.7, exactFrameRatio: 1 },
  ];
  const session = {
    async collectQuality(id, scenes, options) {
      calls.push({ id, scenes, options });
      return captures[calls.length - 1];
    },
  };
  const result = await collectQualityRepetitions(session, "lane-recovery-control", ["source-code"], {
    repetitions: 3, maxSamples: 1, retainWorst: 3,
  });
  assert.deepEqual(calls.map(call => call.id), [
    "lane-recovery-control-r1", "lane-recovery-control-r2", "lane-recovery-control-r3",
  ]);
  assert.ok(calls.every(call => !Object.hasOwn(call.options, "repetitions")));
  assert.equal(result.repetitionCount, 3);
  assert.equal(result.vmafP5, 53);
  assert.equal(result.ssimP5, 0.91);
  assert.equal(result.ocrCer, 0.14);
  assert.equal(result.exactFrameRatio, 1);
  assert.equal(result.noise.vmafP5, 1);
});

test("lane recovery quality uses an interleaved sham-loss-loss-sham comparison", async () => {
  const calls = [];
  const quality = [95, 90, 89, 88, 87, 86].map((vmafP5, index) => ({
    id: `q${index}`,
    vmafP5,
    ssimP5: 0.95 - index * 0.001,
    worstTileSsim: 0.82 - index * 0.001,
    edgeFidelity: 0.74 - index * 0.001,
    colorError: 2 + index * 0.1,
    ocrCer: 0.05 + index * 0.001,
    blockArtifactSeverity: 3 + index * 0.1,
    exactFrameRatio: 1,
  }));
  const session = {
    async collectQuality(id, scenes, options) {
      calls.push({ type: "quality", id, scenes, options });
      return quality.shift();
    },
    async collectWssLaneRecovery(options) {
      calls.push({ type: "loss", options });
      return { accepted: true, reasons: [], recoveryMs: 1_700 };
    },
  };
  const waits = [];
  const result = await collectLaneRecoveryQualitySequence(session, ["source-code"], {
    qualityRepetitions: 2,
    maxSamples: 1,
    lossDurationMs: 1_500,
    maxRecoveryMs: 5_000,
    waiter: async durationMs => waits.push(durationMs),
  });
  assert.deepEqual(result.order, ["sham", "loss", "loss", "sham"]);
  assert.deepEqual(calls.map(call => call.type), [
    "quality", "quality", "quality", "loss", "quality", "loss", "quality", "quality",
  ]);
  assert.deepEqual(calls.filter(call => call.type === "quality").map(call => call.id), [
    "lane-recovery-warmup",
    "lane-recovery-q0",
    "lane-recovery-q1-sham",
    "lane-recovery-q2-loss",
    "lane-recovery-q3-loss",
    "lane-recovery-q4-sham",
  ]);
  assert.deepEqual(waits, [3_500, 3_500]);
  assert.equal(result.sham.quality.accepted, true);
  assert.equal(result.loss.quality.accepted, true);
  assert.equal(result.accepted, true);
  assert.deepEqual(result.reasons, []);
  assert.equal(result.warmup.vmafP5, 95);
  assert.equal(result.captures.includes(result.warmup), false);
});

test("lane recovery quality fails closed when the sham comparison is unstable", async () => {
  const captures = [95, 90, 85, 84, 83, 78].map(vmafP5 => ({
    vmafP5,
    ssimP5: 0.95,
    worstTileSsim: 0.82,
    edgeFidelity: 0.74,
    colorError: 2,
    ocrCer: 0.05,
    blockArtifactSeverity: 3,
    exactFrameRatio: 1,
  }));
  const session = {
    async collectQuality() { return captures.shift(); },
    async collectWssLaneRecovery() {
      return { accepted: true, reasons: [], recoveryMs: 1_700 };
    },
  };
  const result = await collectLaneRecoveryQualitySequence(session, ["source-code"], {
    qualityRepetitions: 2,
    waiter: async () => {},
  });
  assert.equal(result.accepted, false);
  assert.ok(result.reasons.includes("sham-unstable:vmafP5-regression"));
});

test("single-lane recovery requires live parity reconstruction and a healthy bounded return", () => {
  const before = {
    resyncs: 4,
    mediaGeneration: 12,
    wssMedia: {
      protocol: 3,
      topology: { laneCount: 16, dataShards: 15, parityShards: 1 },
      openLanes: 16,
      reconstructed: 100,
      presented: 100,
      recoveredFrames: 10,
      dependencyLosses: 0,
      recoveryState: "healthy",
      highestSequenceByLane: Array(16).fill(100),
    },
  };
  const during = {
    ...before,
    wssMedia: {
      ...before.wssMedia,
      openLanes: 15,
      reconstructed: 120,
      presented: 120,
      recoveredFrames: 30,
      highestSequenceByLane: [100, ...Array(15).fill(120)],
    },
  };
  const after = {
    ...before,
    wssMedia: {
      ...before.wssMedia,
      reconstructed: 140,
      presented: 140,
      recoveredFrames: 31,
      highestSequenceByLane: [139, ...Array(15).fill(140)],
    },
  };
  const result = evaluateWssLaneRecovery(before, during, after, { recoveryMs: 1_240, maxRecoveryMs: 5_000 });
  assert.equal(result.accepted, true);
  assert.equal(result.reconstructedDuringLoss, 20);
  assert.equal(result.presentedDuringLoss, 20);
  assert.equal(result.recoveredFramesDuringLoss, 20);
  assert.deepEqual(result.reasons, []);
  const duplicateParity = { laneCount: 16, dataShards: 14, parityShards: 2 };
  assert.equal(evaluateWssLaneRecovery(
    { ...before, wssMedia: { ...before.wssMedia, topology: duplicateParity } },
    { ...during, wssMedia: { ...during.wssMedia, topology: duplicateParity } },
    { ...after, wssMedia: { ...after.wssMedia, topology: duplicateParity } },
    { recoveryMs: 1_240, maxRecoveryMs: 5_000 },
  ).accepted, true);
});

test("single-lane recovery rejects decoder resets, dependency loss, and missing parity use", () => {
  const before = {
    resyncs: 1,
    mediaGeneration: 7,
    wssMedia: {
      protocol: 3,
      topology: { laneCount: 16, dataShards: 15, parityShards: 1 },
      openLanes: 16,
      reconstructed: 10,
      presented: 10,
      recoveredFrames: 2,
      dependencyLosses: 0,
      recoveryState: "healthy",
      highestSequenceByLane: Array(16).fill(10),
    },
  };
  const during = {
    ...before,
    wssMedia: {
      ...before.wssMedia,
      openLanes: 15,
      reconstructed: 10,
      presented: 10,
      highestSequenceByLane: [10, ...Array(15).fill(11)],
    },
  };
  const after = {
    ...before,
    resyncs: 2,
    mediaGeneration: 8,
    wssMedia: {
      ...before.wssMedia,
      reconstructed: 11,
      presented: 11,
      dependencyLosses: 1,
      highestSequenceByLane: Array(16).fill(12),
    },
  };
  const result = evaluateWssLaneRecovery(before, during, after, { recoveryMs: 6_000, maxRecoveryMs: 5_000 });
  assert.equal(result.accepted, false);
  assert.deepEqual(result.reasons.sort(), [
    "decoder-reset",
    "dependency-loss",
    "media-generation-changed",
    "no-parity-reconstruction-during-loss",
    "no-presented-frame-during-loss",
    "recovery-timeout",
  ]);
});

test("single-lane recovery rejects a reopened lane that never rejoins the sequence frontier", () => {
  const topology = { laneCount: 6, dataShards: 5, parityShards: 1 };
  const before = {
    resyncs: 0,
    mediaGeneration: 2,
    wssMedia: {
      protocol: 3,
      topology,
      openLanes: 6,
      reconstructed: 100,
      presented: 100,
      recoveredFrames: 80,
      dependencyLosses: 0,
      recoveryState: "healthy",
      highestSequenceByLane: [100, 100, 100, 100, 100, 100],
    },
  };
  const during = {
    ...before,
    wssMedia: {
      ...before.wssMedia,
      openLanes: 5,
      reconstructed: 120,
      presented: 120,
      recoveredFrames: 100,
      highestSequenceByLane: [100, 120, 120, 120, 120, 120],
    },
  };
  const after = {
    ...before,
    wssMedia: {
      ...before.wssMedia,
      reconstructed: 140,
      presented: 140,
      recoveredFrames: 120,
      highestSequenceByLane: [100, 140, 140, 140, 140, 140],
    },
  };

  const result = evaluateWssLaneRecovery(before, during, after, {
    stripe: 0,
    recoveryMs: 1_700,
    maxRecoveryMs: 5_000,
  });

  assert.equal(result.accepted, false);
  assert.ok(result.reasons.includes("returning-lane-stalled"));
});

test("WSS cohort admission requires consecutive recovery-free quorum progress", () => {
  assert.equal(typeof liveModule.advanceWssCohortAdmission, "function");
  const topology = { laneCount: 16, dataShards: 15, parityShards: 1 };
  const snapshot = (sequence, options = {}) => ({
    route: "wss",
    resyncs: options.resyncs ?? 2,
    renderedAgeMs: options.renderedAgeMs ?? 80,
    mediaGeneration: options.mediaGeneration ?? 7,
    signaling: {
      registered: true,
      registeredAtMs: options.registeredAtMs ?? 1_000,
    },
    wssMedia: {
      protocol: 3,
      topology,
      openLanes: options.openLanes ?? 15,
      reconstructed: options.reconstructed ?? sequence,
      presented: options.presented ?? sequence,
      dependencyLosses: options.dependencyLosses ?? 0,
      recoveryState: options.recoveryState ?? "healthy",
      highestSequenceByLane: [
        ...Array(15).fill(sequence),
        options.paritySequence ?? 0,
      ],
    },
  });

  let state = liveModule.advanceWssCohortAdmission(null, snapshot(100), {
    requiredProgressSamples: 3,
  });
  assert.equal(state.accepted, false);
  assert.equal(state.consecutiveProgressSamples, 0);

  state = liveModule.advanceWssCohortAdmission(state, snapshot(110), {
    requiredProgressSamples: 3,
  });
  assert.equal(state.consecutiveProgressSamples, 1);

  state = liveModule.advanceWssCohortAdmission(state, snapshot(120, { resyncs: 3 }), {
    requiredProgressSamples: 3,
  });
  assert.equal(state.accepted, false);
  assert.equal(state.consecutiveProgressSamples, 0);
  assert.ok(state.observation.reasons.includes("decoder-resync"));

  state = liveModule.advanceWssCohortAdmission(state, snapshot(130, { resyncs: 3 }), {
    requiredProgressSamples: 3,
  });
  state = liveModule.advanceWssCohortAdmission(state, snapshot(140, { resyncs: 3 }), {
    requiredProgressSamples: 3,
  });
  state = liveModule.advanceWssCohortAdmission(state, snapshot(150, { resyncs: 3 }), {
    requiredProgressSamples: 3,
  });
  assert.equal(state.accepted, true);
  assert.equal(state.consecutiveProgressSamples, 3);
  assert.equal(state.observation.advancedLanes, 15);

  const staleProgress = liveModule.evaluateWssCohortProgress(
    snapshot(150, { resyncs: 3 }),
    snapshot(160, { resyncs: 3, renderedAgeMs: 750 }),
  );
  assert.equal(staleProgress.accepted, false);
  assert.ok(staleProgress.reasons.includes("rendered-frame-stale"));
});

test("WSS cohort continuity classifies a post-admission recovery as infrastructure loss", () => {
  assert.equal(typeof liveModule.evaluateWssCohortContinuity, "function");
  const topology = { laneCount: 16, dataShards: 15, parityShards: 1 };
  const admitted = {
    route: "wss",
    resyncs: 4,
    renderedAgeMs: 90,
    mediaGeneration: 9,
    signaling: { registered: true, registeredAtMs: 2_000 },
    wssMedia: {
      protocol: 3,
      topology,
      openLanes: 16,
      reconstructed: 200,
      presented: 200,
      dependencyLosses: 0,
      recoveryState: "healthy",
      highestSequenceByLane: Array(16).fill(200),
    },
  };
  const recovered = {
    ...admitted,
    resyncs: 5,
    renderedAgeMs: 1_200,
    signaling: { registered: true, registeredAtMs: 2_500 },
    wssMedia: {
      ...admitted.wssMedia,
      reconstructed: 210,
      presented: 205,
      dependencyLosses: 1,
      highestSequenceByLane: Array(16).fill(210),
    },
  };

  const continuity = liveModule.evaluateWssCohortContinuity(admitted, recovered);
  assert.equal(continuity.accepted, false);
  assert.deepEqual(continuity.reasons.sort(), [
    "decoder-resync",
    "dependency-loss",
    "signaling-epoch-changed",
  ]);
});

test("WSS cohort continuity accepts an intentionally paused static frame", () => {
  assert.equal(typeof liveModule.evaluateWssCohortContinuity, "function");
  const topology = { laneCount: 16, dataShards: 15, parityShards: 1 };
  const admitted = {
    route: "wss",
    resyncs: 2,
    renderedAgeMs: 80,
    mediaGeneration: 7,
    signaling: { registered: true, registeredAtMs: 1_000 },
    wssMedia: {
      protocol: 3,
      topology,
      openLanes: 16,
      reconstructed: 415,
      presented: 415,
      dependencyLosses: 0,
      recoveryState: "healthy",
      highestSequenceByLane: Array(16).fill(415),
    },
  };
  const paused = {
    ...admitted,
    renderedAgeMs: 835,
  };

  const continuity = liveModule.evaluateWssCohortContinuity(admitted, paused);
  assert.equal(continuity.accepted, true);
  assert.deepEqual(continuity.reasons, []);
  assert.equal(continuity.renderedAgeMs, 835);
});

test("WSS cohort continuity permits only an explicitly expected media generation transition", () => {
  const topology = { laneCount: 16, dataShards: 15, parityShards: 1 };
  const admitted = {
    route: "wss",
    resyncs: 2,
    renderedAgeMs: 80,
    mediaGeneration: 7,
    signaling: { registered: true, registeredAtMs: 1_000 },
    wssMedia: {
      protocol: 3,
      topology,
      openLanes: 16,
      reconstructed: 415,
      presented: 415,
      dependencyLosses: 0,
      recoveryState: "healthy",
      highestSequenceByLane: Array(16).fill(415),
    },
  };
  const transitioned = {
    ...admitted,
    mediaGeneration: 8,
    wssMedia: {
      ...admitted.wssMedia,
      reconstructed: 2,
      highestSequenceByLane: Array(16).fill(2),
    },
  };

  assert.deepEqual(
    liveModule.evaluateWssCohortContinuity(admitted, transitioned).reasons,
    ["media-generation-changed"],
  );
  assert.equal(liveModule.evaluateWssCohortContinuity(admitted, transitioned, {
    allowMediaGenerationChange: true,
  }).accepted, true);
  assert.equal(liveModule.evaluateWssCohortContinuity(admitted, {
    ...transitioned,
    wssMedia: { ...transitioned.wssMedia, dependencyLosses: 1 },
  }, {
    allowMediaGenerationChange: true,
  }).accepted, false);
});

test("host publishes benchmark status before candidate encoding can skip the producer tail", () => {
  const source = readFileSync(join(import.meta.dirname, "..", "..", "host", "src", "main.rs"), "utf8");
  const session = source.slice(source.indexOf("'session: loop {"), source.indexOf("fn run_agent_mode"));
  const status = session.indexOf(">= next_benchmark_status");
  const encode = session.indexOf("encode_texture_with_damage(scaled.texture, frame_damage)");
  assert.ok(status >= 0, "benchmark status scheduler must remain in the session loop");
  assert.ok(encode >= 0, "local NVENC encode must remain in the session loop");
  assert.ok(status < encode, "benchmark status must not be starved by an encode-error continue");
  const scheduler = session.slice(status, session.indexOf("capture.tick(loop_now)", status));
  assert.equal(scheduler.includes("wss_media.is_some()"), false,
    "profile activation telemetry must not wait for the transport event");
});

test("aggressive optimizer requires a material quality win even when transport gets cheaper", () => {
  const baseline = {
    firstFrameMs: 100, presentedFps: 30, latencyP95Ms: 180, freezes: 0, decoderResets: 0,
    dependencyLosses: 0, spatialTransitions: 0, queueSlopePerMinute: 0,
    vmafMean: 80, vmafP5: 44, ssimMean: 0.95, ssimP5: 0.89, worstTileSsim: 0.4,
    edgeFidelity: 0.68, colorError: 3, ocrCer: 0.56, unchangedRegionError: 0.1,
    blockArtifactSeverity: 30, exactFrameRatio: 1, duplicateFrames: 0, droppedFrames: 0,
    freezeCount: 0, presentationCadenceP95Ms: 80, wireBitrateMbps: 4,
    noise: {},
  };
  const cheaperButEquallyBad = { ...baseline, wireBitrateMbps: 3.5 };
  const comparison = evaluateOptimizationRun(baseline, cheaperButEquallyBad, {});
  assert.equal(comparison.accepted, false);
  assert.ok(comparison.reasons.includes("no-material-quality-improvement"));
});

test("activity-contaminated performance remains visible but cannot veto a quality result", () => {
  const baseline = {
    firstFrameMs: 100, presentedFps: 30, latencyP95Ms: 150, freezes: 0, decoderResets: 0,
    dependencyLosses: 0, spatialTransitions: 0, queueSlopePerMinute: 0,
    vmafMean: 80, vmafP5: 44, ssimMean: 0.95, ssimP5: 0.89, worstTileSsim: 0.4,
    edgeFidelity: 0.68, colorError: 3, ocrCer: 0.56, unchangedRegionError: 0.1,
    blockArtifactSeverity: 4.5, exactFrameRatio: 1, duplicateFrames: 0, droppedFrames: 0,
    freezeCount: 0, presentationCadenceP95Ms: 80, wireBitrateMbps: 4, noise: {},
  };
  const candidate = { ...baseline, vmafP5: 50, presentedFps: 20, latencyP95Ms: 900,
    performanceContaminated: true, latencyAccuracy: "possibly-inaccurate" };
  const comparison = evaluateOptimizationRun(baseline, candidate, {});
  assert.equal(comparison.accepted, true);
  assert.deepEqual(comparison.hardGates, []);
  assert.deepEqual(comparison.advisoryHardGates.sort(), ["low-presented-fps", "stream-latency"]);
  assert.equal(comparison.performanceValidation, "possibly-inaccurate");
});

test("ROI candidates fail closed when the measured window applied no QP maps", () => {
  const baseline = {
    firstFrameMs: 100, presentedFps: 30, latencyP95Ms: 150, freezes: 0, decoderResets: 0,
    dependencyLosses: 0, spatialTransitions: 0, queueSlopePerMinute: 0,
    vmafMean: 80, vmafP5: 44, ssimMean: 0.95, ssimP5: 0.89, worstTileSsim: 0.4,
    edgeFidelity: 0.68, colorError: 3, ocrCer: 0.56, unchangedRegionError: 0.1,
    blockArtifactSeverity: 4.5, exactFrameRatio: 1, duplicateFrames: 0, droppedFrames: 0,
    freezeCount: 0, presentationCadenceP95Ms: 80, wireBitrateMbps: 4, noise: {},
  };
  const candidate = {
    ...baseline,
    configuration: { roiMode: "damage" },
    vmafP5: 50,
    deltaCount: 300,
    qpMapFrames: 0,
    qpMapChangedCtbs: 0,
    qpMapTotalCtbs: 0,
  };
  const comparison = evaluateOptimizationRun(baseline, candidate, {});
  assert.equal(comparison.accepted, false);
  assert.ok(comparison.reasons.includes("roi-activity-missing"));
});

test("ROI candidates require at least thirty sequence-proven mapped delta frames", () => {
  const baseline = {
    firstFrameMs: 100, presentedFps: 30, latencyP95Ms: 150, freezes: 0, decoderResets: 0,
    dependencyLosses: 0, spatialTransitions: 0, queueSlopePerMinute: 0,
    vmafMean: 80, vmafP5: 44, ssimMean: 0.95, ssimP5: 0.89, worstTileSsim: 0.4,
    edgeFidelity: 0.68, colorError: 3, ocrCer: 0.56, unchangedRegionError: 0.1,
    blockArtifactSeverity: 4.5, exactFrameRatio: 1, duplicateFrames: 0, droppedFrames: 0,
    freezeCount: 0, presentationCadenceP95Ms: 80, wireBitrateMbps: 4, noise: {},
  };
  const candidate = {
    ...baseline,
    configuration: { roiMode: "damage" },
    vmafP5: 50,
    qpMapFrames: 12,
    roiActivity: { mode: "damage", mappedFrames: 12, lastSequence: 44, lastReason: "applied" },
  };
  const comparison = evaluateOptimizationRun(baseline, candidate, {});
  assert.equal(comparison.accepted, false);
  assert.ok(comparison.reasons.includes("roi-activity-insufficient"));
});

test("GPU temporal ROI candidates reject analysis overhead above 1.5 ms p95", () => {
  const baseline = {
    firstFrameMs: 100, presentedFps: 30, latencyP95Ms: 150, freezes: 0, decoderResets: 0,
    dependencyLosses: 0, spatialTransitions: 0, queueSlopePerMinute: 0,
    vmafMean: 80, vmafP5: 44, ssimMean: 0.95, ssimP5: 0.89, worstTileSsim: 0.4,
    edgeFidelity: 0.68, colorError: 3, ocrCer: 0.56, unchangedRegionError: 0.1,
    blockArtifactSeverity: 4.5, exactFrameRatio: 1, duplicateFrames: 0, droppedFrames: 0,
    freezeCount: 0, presentationCadenceP95Ms: 80, wireBitrateMbps: 4, noise: {},
  };
  const candidate = {
    ...baseline,
    configuration: { roiMode: "damage" },
    vmafP5: 50,
    qpMapFrames: 40,
    roiActivity: {
      mode: "damage", mappedFrames: 40, source: "gpu-temporal", temporalFrames: 50,
      temporalPrimingFrames: 1, analysisP95Us: 1_501, analysisMaxUs: 1_900,
    },
  };
  const comparison = evaluateOptimizationRun(baseline, candidate, {});
  assert.equal(comparison.accepted, false);
  assert.ok(comparison.reasons.includes("temporal-analysis-overhead"));
});

test("feedback ROI candidates require active exact N-1 reconstruction under the 2 ms budget", () => {
  const baseline = {
    firstFrameMs: 100, presentedFps: 30, latencyP95Ms: 150, freezes: 0, decoderResets: 0,
    dependencyLosses: 0, spatialTransitions: 0, queueSlopePerMinute: 0,
    vmafMean: 80, vmafP5: 44, ssimMean: 0.95, ssimP5: 0.89, worstTileSsim: 0.4,
    edgeFidelity: 0.68, colorError: 3, ocrCer: 0.56, unchangedRegionError: 0.1,
    blockArtifactSeverity: 4.5, exactFrameRatio: 1, duplicateFrames: 0, droppedFrames: 0,
    freezeCount: 0, presentationCadenceP95Ms: 80, wireBitrateMbps: 4, noise: {},
  };
  const activity = {
    mode: "feedback", mappedFrames: 20, source: "gpu-temporal", temporalFrames: 50,
    temporalPrimingFrames: 1, analysisP95Us: 800, analysisMaxUs: 1_100,
    reconstructionActive: true, feedbackSubmittedFrames: 49, feedbackReadyFrames: 45,
    feedbackLateFrames: 2, feedbackSkippedFrames: 0, feedbackAdjustedFrames: 20,
    feedbackWarmupFrames: 10, feedbackMotionBypasses: 0, feedbackMissingFrames: 5,
    feedbackEligibleCtbs: 160, feedbackRefinedCtbs: 20, feedbackFundedCtbs: 40,
    feedbackSubmitP95Us: 80, feedbackSubmitMaxUs: 120,
    feedbackReadbackP95Us: 60, feedbackReadbackMaxUs: 90,
    lastFeedbackSequence: 49, lastFeedbackAgeFrames: 1,
  };
  const candidate = {
    ...baseline, configuration: { roiMode: "feedback" }, vmafP5: 50,
    qpMapFrames: 20, mappedQualitySamples: 1, roiActivity: activity,
  };

  assert.equal(evaluateOptimizationRun(baseline, candidate, {}).accepted, true);
  const inactive = evaluateOptimizationRun(baseline, {
    ...candidate, roiActivity: { ...activity, reconstructionActive: false },
  }, {});
  assert.ok(inactive.reasons.includes("feedback-reconstruction-inactive"));
  const stale = evaluateOptimizationRun(baseline, {
    ...candidate, roiActivity: { ...activity, lastFeedbackAgeFrames: 2 },
  }, {});
  assert.ok(stale.reasons.includes("feedback-exact-sequence-missing"));
  const slow = evaluateOptimizationRun(baseline, {
    ...candidate, roiActivity: { ...activity, feedbackSubmitP95Us: 1_200,
      feedbackReadbackP95Us: 900 },
  }, {});
  assert.ok(slow.reasons.includes("feedback-processing-overhead"));
});

test("feedback-headroom candidates require verified spend activity below the total WSS cap", () => {
  const baseline = {
    firstFrameMs: 100, presentedFps: 30, latencyP95Ms: 150, freezes: 0, decoderResets: 0,
    dependencyLosses: 0, spatialTransitions: 0, queueSlopePerMinute: 0,
    vmafMean: 80, vmafP5: 44, ssimMean: 0.95, ssimP5: 0.89, worstTileSsim: 0.4,
    edgeFidelity: 0.8, ocrCer: 0.2, colorError: 2, unchangedRegionError: 0.01,
    blockArtifactSeverity: 4.5, exactFrameRatio: 1, duplicateFrames: 0, droppedFrames: 0,
    freezeCount: 0, presentationCadenceP95Ms: 80, wireBitrateMbps: 4, noise: {},
  };
  const activity = {
    mode: "feedback-headroom", mappedFrames: 20, source: "gpu-temporal", temporalFrames: 50,
    temporalPrimingFrames: 1, analysisP95Us: 800, analysisMaxUs: 1_100,
    reconstructionActive: true, feedbackSubmittedFrames: 49, feedbackReadyFrames: 45,
    feedbackLateFrames: 2, feedbackSkippedFrames: 0, feedbackAdjustedFrames: 20,
    feedbackWarmupFrames: 10, feedbackMotionBypasses: 0, feedbackMissingFrames: 5,
    feedbackEligibleCtbs: 160, feedbackRefinedCtbs: 20, feedbackFundedCtbs: 0,
    feedbackSpendHeadroomFrames: 20, feedbackNetZeroFrames: 0,
    feedbackWireOccupancyP95Bps: 2_100_000, feedbackWireOccupancyMaxBps: 2_400_000,
    feedbackSubmitP95Us: 80, feedbackSubmitMaxUs: 120,
    feedbackReadbackP95Us: 60, feedbackReadbackMaxUs: 90,
    lastFeedbackSequence: 49, lastFeedbackAgeFrames: 1,
    lastFeedbackBudgetPolicy: "spend-headroom", lastFeedbackWireOccupancyBps: 1_900_000,
  };
  const candidate = {
    ...baseline, configuration: { roiMode: "feedback-headroom", totalMediaCapMbps: 6 },
    vmafP5: 50, qpMapFrames: 20, mappedQualitySamples: 1, roiActivity: activity,
  };

  assert.equal(evaluateOptimizationRun(baseline, candidate, {}).accepted, true);
  const inactive = evaluateOptimizationRun(baseline, { ...candidate, roiActivity: {
    ...activity, feedbackSpendHeadroomFrames: 0, feedbackNetZeroFrames: 20,
  } }, {});
  assert.ok(inactive.reasons.includes("feedback-headroom-inactive"));
  const overCap = evaluateOptimizationRun(baseline, { ...candidate, roiActivity: {
    ...activity, feedbackWireOccupancyMaxBps: 6_100_000,
  } }, {});
  assert.ok(overCap.reasons.includes("feedback-headroom-cap-risk"));
});

test("ROI candidates require at least one exact mapped delta quality sample", () => {
  const baseline = {
    firstFrameMs: 100, presentedFps: 30, latencyP95Ms: 150, freezes: 0, decoderResets: 0,
    dependencyLosses: 0, spatialTransitions: 0, queueSlopePerMinute: 0,
    vmafMean: 80, vmafP5: 44, ssimMean: 0.95, ssimP5: 0.89, worstTileSsim: 0.4,
    edgeFidelity: 0.68, colorError: 3, ocrCer: 0.56, unchangedRegionError: 0.1,
    blockArtifactSeverity: 4.5, exactFrameRatio: 1, duplicateFrames: 0, droppedFrames: 0,
    freezeCount: 0, presentationCadenceP95Ms: 80, wireBitrateMbps: 4, noise: {},
  };
  const candidate = {
    ...baseline,
    configuration: { roiMode: "damage" },
    qpMapFrames: 40,
    mappedQualitySamples: 0,
    roiActivity: {
      mode: "damage", mappedFrames: 40, source: "gpu-temporal", temporalFrames: 50,
      temporalPrimingFrames: 2, analysisP95Us: 220, analysisMaxUs: 1_598,
    },
  };
  const comparison = evaluateOptimizationRun(baseline, candidate, {});
  assert.equal(comparison.accepted, false);
  assert.ok(comparison.reasons.includes("roi-quality-activity-missing"));
});

test("hybrid candidates require tile-affected quality samples and zero stale composites", () => {
  const baseline = {
    firstFrameMs: 100, presentedFps: 30, latencyP95Ms: 150, freezes: 0, decoderResets: 0,
    dependencyLosses: 0, spatialTransitions: 0, queueSlopePerMinute: 0,
    vmafMean: 80, vmafP5: 44, ssimMean: 0.95, ssimP5: 0.89, worstTileSsim: 0.4,
    edgeFidelity: 0.68, colorError: 3, ocrCer: 0.56, unchangedRegionError: 0.1,
    blockArtifactSeverity: 4.5, exactFrameRatio: 1, duplicateFrames: 0, droppedFrames: 0,
    freezeCount: 0, presentationCadenceP95Ms: 80, wireBitrateMbps: 4, noise: {},
  };
  const candidate = {
    ...baseline,
    configuration: { hybridMode: "png-static-roi" },
    hybridQualitySamples: 0,
    hybridActivity: { staleComposites: 1 },
  };
  const comparison = evaluateOptimizationRun(baseline, candidate, {});
  assert.ok(comparison.reasons.includes("hybrid-quality-activity-missing"));
  assert.ok(comparison.reasons.includes("hybrid-stale-composite"));
});

test("quiescent screening fails closed on missing, bypassed, or coalesced immediate presentation", () => {
  const baseline = {
    firstFrameMs: 100, presentedFps: 30, latencyP95Ms: 150, freezes: 0, decoderResets: 0,
    dependencyLosses: 0, spatialTransitions: 0, queueSlopePerMinute: 0,
    vmafMean: 80, vmafP5: 44, ssimMean: 0.95, ssimP5: 0.89, worstTileSsim: 0.4,
    edgeFidelity: 0.68, colorError: 3, ocrCer: 0.56, unchangedRegionError: 0.1,
    blockArtifactSeverity: 4.5, exactFrameRatio: 1, duplicateFrames: 0, droppedFrames: 0,
    freezeCount: 0, presentationCadenceP95Ms: 80, wireBitrateMbps: 4, noise: {},
  };
  const candidate = {
    ...baseline,
    configuration: { hybridMode: "png-quiescent-detail" },
    vmafP5: 50,
    hybridQualitySamples: 1,
    hybridActivity: { staleComposites: 0 },
    hostHybridActivity: { frames: 30, analysisP95Us: 100 },
  };

  const missing = evaluateOptimizationRun(baseline, candidate, {});
  assert.ok(missing.reasons.includes("wss-presentation-telemetry-missing"));

  const lossy = evaluateOptimizationRun(baseline, {
    ...candidate,
    wssPresentation: {
      protocol: 1,
      decodedFrames: 240,
      presentedFrames: 195,
      immediateAttempts: 230,
      immediatePresented: 195,
      rafEnqueued: 10,
      rafCoalesced: 44,
      rafLateDrops: 1,
    },
  }, {});
  for (const reason of [
    "wss-immediate-path-bypass",
    "wss-immediate-presentation-loss",
    "wss-raf-enqueue",
    "wss-raf-coalescing",
    "wss-raf-late-drop",
  ]) assert.ok(lossy.reasons.includes(reason), reason);
});

test("quiescent async validation fails closed on slow, unavailable, or failed collection", () => {
  const baseline = {
    firstFrameMs: 100, presentedFps: 30, latencyP95Ms: 150, freezes: 0, decoderResets: 0,
    dependencyLosses: 0, spatialTransitions: 0, queueSlopePerMinute: 0,
    vmafMean: 80, vmafP5: 44, ssimMean: 0.95, ssimP5: 0.89, worstTileSsim: 0.4,
    edgeFidelity: 0.68, colorError: 3, ocrCer: 0.56, unchangedRegionError: 0.1,
    blockArtifactSeverity: 4.5, exactFrameRatio: 1, duplicateFrames: 0, droppedFrames: 0,
    freezeCount: 0, presentationCadenceP95Ms: 80, wireBitrateMbps: 4, noise: {},
  };
  const candidate = {
    ...baseline,
    configuration: { hybridMode: "png-quiescent-detail" },
    vmafP5: 50,
    hybridQualitySamples: 1,
    hybridActivity: { staleComposites: 0 },
    hostHybridActivity: {
      frames: 30, analysisP95Us: 1_200, validationFrames: 30,
      validationCollectP95Us: 201, validationUnavailableFrames: 1,
      validationFailedFrames: 1,
    },
  };

  const comparison = evaluateOptimizationRun(baseline, candidate, {});
  assert.ok(comparison.reasons.includes("quiescent-validation-collect-overhead"));
  assert.ok(comparison.reasons.includes("quiescent-validation-unavailable"));
  assert.ok(comparison.reasons.includes("quiescent-validation-failed"));
});

test("quiescent async validation admits bounded pending readback after exact resolution", () => {
  const baseline = {
    firstFrameMs: 100, presentedFps: 30, latencyP95Ms: 150, freezes: 0, decoderResets: 0,
    dependencyLosses: 0, spatialTransitions: 0, queueSlopePerMinute: 0,
    vmafMean: 80, vmafP5: 44, ssimMean: 0.95, ssimP5: 0.89, worstTileSsim: 0.4,
    edgeFidelity: 0.68, colorError: 3, ocrCer: 0.56, unchangedRegionError: 0.1,
    blockArtifactSeverity: 4.5, exactFrameRatio: 1, duplicateFrames: 0, droppedFrames: 0,
    freezeCount: 0, presentationCadenceP95Ms: 80, wireBitrateMbps: 4, noise: {},
  };
  const candidate = {
    ...baseline,
    configuration: { hybridMode: "png-quiescent-detail" },
    vmafP5: 50,
    hybridQualitySamples: 1,
    hybridActivity: { staleComposites: 0 },
    wssPresentation: {
      protocol: 1, decodedFrames: 240, presentedFrames: 240,
      immediateAttempts: 240, immediatePresented: 240,
      rafEnqueued: 0, rafCoalesced: 0, rafLateDrops: 0,
    },
    hostHybridActivity: {
      mode: "png-quiescent-detail", frames: 60, analysisP95Us: 120,
      validationFrames: 60, validationCollectP95Us: 10,
      validationUnavailableFrames: 20, validationFailedFrames: 0,
      deferredValidationReadyFrames: 40, deferredValidationPendingFrames: 40,
      deferredValidationAgeP95Frames: 2, deferredValidationAgeMaxFrames: 2,
      deferredValidationPendingMaxAgeFrames: 1,
      suppressions: 80, reactivations: 80, permanentInvalidations: 0,
      staleReactivationRejects: 0, pngBytesAvoided: 400_000,
    },
  };

  const comparison = evaluateOptimizationRun(baseline, candidate, {});
  assert.equal(comparison.accepted, true);
  assert.ok(!comparison.reasons.includes("quiescent-validation-unavailable"));
  assert.ok(!comparison.reasons.includes("quiescent-validation-age"));
  assert.ok(!comparison.reasons.includes("quiescent-validation-unresolved"));
});

test("experiment aggregation cannot hide one failed repetition behind the median", () => {
  const repetitions = [
    { firstFrameMs: 100, presentedFps: 30, latencyP95Ms: 100, encodeP95Ms: 4, frameGapP95Ms: 40,
      vmafP5: 90, worstTileSsim: 0.9, ocrCer: 0.05, freezes: 0 },
    { firstFrameMs: 120, presentedFps: 30, latencyP95Ms: 110, encodeP95Ms: 5, frameGapP95Ms: 45,
      vmafP5: 89, worstTileSsim: 0.88, ocrCer: 0.06, freezes: 0 },
    { firstFrameMs: 6_000, presentedFps: 20, latencyP95Ms: 700, encodeP95Ms: 35, frameGapP95Ms: 800,
      vmafP5: 60, worstTileSsim: 0.4, ocrCer: 0.4, freezes: 1 },
  ];
  const aggregate = aggregateExperimentRepetitions(repetitions, { id: "candidate" });
  assert.equal(aggregate.firstFrameMs, 6_000);
  assert.equal(aggregate.presentedFps, 20);
  assert.equal(aggregate.latencyP95Ms, 700);
  assert.equal(aggregate.encodeP95Ms, 35);
  assert.equal(aggregate.frameGapP95Ms, 800);
  assert.equal(aggregate.vmafP5, 60);
  assert.equal(aggregate.worstTileSsim, 0.4);
  assert.equal(aggregate.ocrCer, 0.4);
  assert.equal(aggregate.freezes, 1);
});

test("experiment aggregation preserves any immediate-presentation loss across repetitions", () => {
  const repetitions = [
    { wssPresentation: { protocol: 1, decodedFrames: 30, presentedFrames: 30,
      immediateAttempts: 30, immediatePresented: 30, rafEnqueued: 0, rafCoalesced: 0, rafLateDrops: 0 } },
    { wssPresentation: { protocol: 1, decodedFrames: 30, presentedFrames: 25,
      immediateAttempts: 30, immediatePresented: 25, rafEnqueued: 0, rafCoalesced: 5, rafLateDrops: 0 } },
    { wssPresentation: { protocol: 1, decodedFrames: 30, presentedFrames: 30,
      immediateAttempts: 30, immediatePresented: 30, rafEnqueued: 0, rafCoalesced: 0, rafLateDrops: 0 } },
  ];

  const aggregate = aggregateExperimentRepetitions(repetitions, { id: "candidate" });
  assert.deepEqual(aggregate.wssPresentation, {
    protocol: 1,
    decodedFrames: 90,
    presentedFrames: 85,
    immediateAttempts: 90,
    immediatePresented: 85,
    rafEnqueued: 0,
    rafCoalesced: 5,
    rafLateDrops: 0,
  });
});

test("experiment aggregation never promotes unavailable bandwidth evidence to verified", () => {
  const unknown = aggregateExperimentRepetitions([
    { bandwidthVerified: null },
    { bandwidthVerified: true },
  ], { id: "candidate" });
  assert.equal(unknown.bandwidthVerified, null);

  const verified = aggregateExperimentRepetitions([
    { bandwidthVerified: true },
    { bandwidthVerified: true },
  ], { id: "candidate" });
  assert.equal(verified.bandwidthVerified, true);

  const rejected = aggregateExperimentRepetitions([
    { bandwidthVerified: true },
    { bandwidthVerified: false },
  ], { id: "candidate" });
  assert.equal(rejected.bandwidthVerified, false);
});

test("experiment aggregation retains quiescent cache efficiency and bounded-age evidence", () => {
  const common = {
    firstFrameMs: 100, presentedFps: 30, latencyP95Ms: 100, encodeP95Ms: 4,
    frameGapP95Ms: 40, vmafP5: 90, worstTileSsim: 0.9, ocrCer: 0.05,
  };
  const aggregate = aggregateExperimentRepetitions([
    { ...common, hostHybridActivity: {
      mode: "png-quiescent-detail", frames: 300, upserts: 4, invalidations: 3,
      pngBytes: 20_000, pngUsTotal: 4_000, sidecarBytes: 24_000,
      suppressions: 120, reactivations: 116, permanentInvalidations: 4,
      staleReactivationRejects: 0, pngBytesAvoided: 600_000, cachedBytesRetained: 32_000,
      deferredValidationReadyFrames: 80, deferredValidationPendingFrames: 76,
      deferredValidationAgeP95Frames: 2, deferredValidationAgeMaxFrames: 2,
      deferredValidationPendingMaxAgeFrames: 1,
    } },
    { ...common, hostHybridActivity: {
      mode: "png-quiescent-detail", frames: 320, upserts: 6, invalidations: 5,
      pngBytes: 24_000, pngUsTotal: 5_000, sidecarBytes: 29_000,
      suppressions: 130, reactivations: 125, permanentInvalidations: 5,
      staleReactivationRejects: 1, pngBytesAvoided: 700_000, cachedBytesRetained: 36_000,
      deferredValidationReadyFrames: 90, deferredValidationPendingFrames: 84,
      deferredValidationAgeP95Frames: 2, deferredValidationAgeMaxFrames: 3,
      deferredValidationPendingMaxAgeFrames: 2,
    } },
  ], { id: "cache-suppression" });

  assert.deepEqual({
    frames: aggregate.hostHybridActivity.frames,
    upserts: aggregate.hostHybridActivity.upserts,
    invalidations: aggregate.hostHybridActivity.invalidations,
    pngBytes: aggregate.hostHybridActivity.pngBytes,
    pngUsTotal: aggregate.hostHybridActivity.pngUsTotal,
    sidecarBytes: aggregate.hostHybridActivity.sidecarBytes,
    suppressions: aggregate.hostHybridActivity.suppressions,
    reactivations: aggregate.hostHybridActivity.reactivations,
    permanentInvalidations: aggregate.hostHybridActivity.permanentInvalidations,
    staleReactivationRejects: aggregate.hostHybridActivity.staleReactivationRejects,
    pngBytesAvoided: aggregate.hostHybridActivity.pngBytesAvoided,
    cachedBytesRetained: aggregate.hostHybridActivity.cachedBytesRetained,
    deferredValidationReadyFrames: aggregate.hostHybridActivity.deferredValidationReadyFrames,
    deferredValidationPendingFrames: aggregate.hostHybridActivity.deferredValidationPendingFrames,
    deferredValidationAgeP95Frames: aggregate.hostHybridActivity.deferredValidationAgeP95Frames,
    deferredValidationAgeMaxFrames: aggregate.hostHybridActivity.deferredValidationAgeMaxFrames,
    deferredValidationPendingMaxAgeFrames:
      aggregate.hostHybridActivity.deferredValidationPendingMaxAgeFrames,
  }, {
    frames: 300,
    upserts: 6,
    invalidations: 5,
    pngBytes: 24_000,
    pngUsTotal: 5_000,
    sidecarBytes: 29_000,
    suppressions: 120,
    reactivations: 116,
    permanentInvalidations: 5,
    staleReactivationRejects: 1,
    pngBytesAvoided: 600_000,
    cachedBytesRetained: 32_000,
    deferredValidationReadyFrames: 80,
    deferredValidationPendingFrames: 84,
    deferredValidationAgeP95Frames: 2,
    deferredValidationAgeMaxFrames: 3,
    deferredValidationPendingMaxAgeFrames: 2,
  });
});

test("experiment aggregation records an explicitly skipped action probe", () => {
  const repetitions = [1, 2].map(index => ({
    id: `mouse-neutral-r${index}`,
    firstFrameMs: 100,
    presentedFps: 30,
    latencyP95Ms: 120,
    encodeP95Ms: 5,
    actionProbe: { result: "skipped", reason: "input-automation-disabled" },
  }));

  const aggregate = aggregateExperimentRepetitions(repetitions, { id: "mouse-neutral" });

  assert.deepEqual(aggregate.actionProbe,
    { result: "skipped", reason: "input-automation-disabled" });
});

test("quality pass cannot overwrite live cadence or freeze gates", () => {
  const combined = combinePerformanceAndQuality(
    { frameGapP95Ms: 34, freezes: 0, latencyP95Ms: 120 },
    { presentationCadenceP95Ms: 12_000_000, freezeCount: 7, vmafP5: 88 },
  );
  assert.equal(combined.presentationCadenceP95Ms, 34);
  assert.equal(combined.freezeCount, 0);
  assert.equal(combined.vmafP5, 88);
});

test("WSS canvas presentation records wall-clock frame gaps and performance resets them", () => {
  const client = readClientSource();
  const collector = readFileSync(new URL("../tools/stream-lab-live.mjs", import.meta.url), "utf8");
  assert.match(client, /const frameGap = Math\.max\(0, Math\.round\(renderedAt - lastFrameDrawnAt\)\)/);
  assert.match(client, /hudState\.maxFrameGap = Math\.max/);
  assert.match(client, /lastFrameDrawnAt = null/);
  assert.match(client, /reset\(\) \{[\s\S]*?resetPresentationCounters\(\);/);
  assert.match(collector, /__bubbleplayDiagnostics\?\.reset\?\.\(\)/);
});

test("Stream Lab can pin only its browser requests to an admitted zero-percent Worker version", async () => {
  const live = await import("../tools/stream-lab-live.mjs");
  assert.equal(typeof live.applyWorkerVersionOverride, "function");
  const calls = [];
  const receiver = {
    async call(method, params = {}) {
      calls.push({ method, params });
      return {};
    },
  };
  assert.equal(await live.applyWorkerVersionOverride(receiver, {
    workerName: "playfull-cat",
    versionId: "16275f59-68bd-4a00-8fec-4230a89278ed",
  }), true);
  assert.deepEqual(calls, [
    { method: "Network.enable", params: {} },
    { method: "Network.setCacheDisabled", params: { cacheDisabled: true } },
    {
      method: "Network.setExtraHTTPHeaders",
      params: {
        headers: {
          "Cloudflare-Workers-Version-Overrides":
            'playfull-cat="16275f59-68bd-4a00-8fec-4230a89278ed"',
        },
      },
    },
  ]);
  assert.equal(await live.applyWorkerVersionOverride(receiver, null), false);
  assert.throws(() => live.workerVersionOverrideFromEnv({
    BUBBLEPLAY_WORKER_VERSION_ID: "not-a-version",
    BUBBLEPLAY_WORKER_NAME: "playfull-cat",
  }), /version/i);
});

test("receiver WebSocket diagnostics prove whether the immutable Worker override reached each handshake", () => {
  const listeners = new Map();
  const telemetry = [];
  const receiver = {
    on(method, handler) {
      listeners.set(method, handler);
      return () => listeners.delete(method);
    },
  };
  const dispose = liveModule.installReceiverWebSocketDiagnostics(receiver, {
    appendTelemetry(event) { telemetry.push(event); },
  }, {
    workerName: "playfull-cat",
    versionId: "a34966a1-d238-4dec-8dbe-5405f6a9fdce",
  });
  listeners.get("Network.webSocketCreated")({
    requestId: "signal-1",
    url: "wss://play.example/lab-ws?run=secret-run-id&compat=candidate",
  });
  listeners.get("Network.webSocketWillSendHandshakeRequest")({
    requestId: "signal-1",
    request: {
      url: "wss://play.example/lab-ws?run=secret-run-id&compat=candidate",
      headers: {
        "cloudflare-workers-version-overrides":
          'playfull-cat="a34966a1-d238-4dec-8dbe-5405f6a9fdce"',
      },
    },
  });
  listeners.get("Network.webSocketHandshakeResponseReceived")({
    requestId: "signal-1",
    response: { status: 404 },
  });
  listeners.get("Network.webSocketFrameError")({ requestId: "signal-1", errorMessage: "Unexpected response code: 404" });
  listeners.get("Network.webSocketClosed")({ requestId: "signal-1" });

  assert.deepEqual(telemetry.map(({ at: _at, ...event }) => event), [
    {
      phase: "receiver-websocket-created",
      requestId: "signal-1",
      path: "/lab-ws",
    },
    {
      phase: "receiver-websocket-handshake",
      requestId: "signal-1",
      path: "/lab-ws",
      overrideHeaderPresent: true,
      overrideMatches: true,
    },
    {
      phase: "receiver-websocket-response",
      requestId: "signal-1",
      path: "/lab-ws",
      status: 404,
    },
    {
      phase: "receiver-websocket-error",
      requestId: "signal-1",
      path: "/lab-ws",
      message: "Unexpected response code: 404",
    },
    {
      phase: "receiver-websocket-closed",
      requestId: "signal-1",
      path: "/lab-ws",
    },
  ]);
  assert.equal(telemetry.some(event => JSON.stringify(event).includes("secret-run-id")), false);
  for (const remove of dispose) remove();
  assert.equal(listeners.size, 0);
});

test("Stream Lab rejects missing or mixed receiver assets before measurement", () => {
  const expected = "wss-frame-stage-timing-v2";
  assert.deepEqual(assertReceiverAssetBuild({
    assetBuild: { index: expected, media: expected },
  }, expected), { index: expected, media: expected });
  assert.throws(
    () => assertReceiverAssetBuild({ assetBuild: { index: expected, media: "production" } }, expected),
    error => error?.code === "receiver-asset-build-mismatch" &&
      error?.detail?.actual?.media === "production",
  );
  assert.throws(
    () => assertReceiverAssetBuild({}, expected),
    error => error?.code === "receiver-asset-build-mismatch",
  );
});

test("Stream Lab admits the pinned Worker before validating local receiver diagnostics", async () => {
  const calls = [];
  const session = Object.create(liveModule.LiveStreamLabSession.prototype);
  session.verifyWorkerCompatibility = async () => { calls.push("worker"); };
  session.verifyReceiverCompatibility = async () => { calls.push("receiver"); };
  await session.admitCompatibility();
  assert.deepEqual(calls, ["worker", "receiver"]);
});

test("receiver authentication readiness rejects a stale app document from the prior run", () => {
  const runId = "123e4567-e89b-42d3-a456-426614174000";
  assert.equal(liveModule.receiverAuthenticationReady({ app: true, diagnostics: true, labRun: runId }, runId), true);
  assert.equal(liveModule.receiverAuthenticationReady({ app: true, diagnostics: false, labRun: runId }, runId), false);
  assert.equal(liveModule.receiverAuthenticationReady({
    app: true, diagnostics: true,
    labRun: "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee",
  }, runId), false);
  assert.equal(liveModule.receiverAuthenticationReady({ app: true, diagnostics: true, labRun: null }, runId), false);
  assert.equal(liveModule.receiverAuthenticationReady({ app: false, diagnostics: false, login: true, labRun: runId }, runId), false);
});

test("Stream Lab can fulfill only its document and media module from local version-matched assets", async () => {
  const calls = [];
  let paused = null;
  const receiver = {
    on(method, handler) {
      assert.equal(method, "Fetch.requestPaused");
      paused = handler;
      return () => {};
    },
    async call(method, params = {}) {
      calls.push({ method, params });
      return {};
    },
  };
  assert.equal(await installReceiverAssetOverrides(receiver, {
    baseUrl: "https://playfull-cat.example/",
    indexHtml: "<!doctype html><title>trace</title>",
    mediaJs: 'export const STREAM_LAB_ASSET_BUILD = "trace";',
  }), true);
  assert.equal(calls[0].method, "Fetch.enable");

  await paused({
    requestId: "document",
    resourceType: "Document",
    request: { url: "https://playfull-cat.example/?diag=1" },
  });
  await paused({
    requestId: "media",
    resourceType: "Script",
    request: { url: "https://playfull-cat.example/media.js" },
  });
  await paused({
    requestId: "other",
    resourceType: "Document",
    request: { url: "https://playfull-cat.example/login.html" },
  });

  assert.equal(calls[1].method, "Fetch.fulfillRequest");
  assert.equal(Buffer.from(calls[1].params.body, "base64").toString(), "<!doctype html><title>trace</title>");
  assert.equal(calls[2].method, "Fetch.fulfillRequest");
  assert.match(Buffer.from(calls[2].params.body, "base64").toString(), /STREAM_LAB_ASSET_BUILD/);
  assert.deepEqual(calls[3], {
    method: "Fetch.continueRequest",
    params: { requestId: "other" },
  });
});

test("optimizer selects only gate-passing Pareto candidates with deterministic tie breaking", () => {
  const candidates = [
    { id: "slow", validationAccepted: true, vmafP5: 91, worstTileSsim: 0.91, latencyP95Ms: 190, wireBitrateMbps: 4 },
    { id: "dominated", validationAccepted: true, vmafP5: 90, worstTileSsim: 0.90, latencyP95Ms: 210, wireBitrateMbps: 4.5 },
    { id: "fast", validationAccepted: true, vmafP5: 89, worstTileSsim: 0.89, latencyP95Ms: 120, wireBitrateMbps: 3 },
    { id: "failed", validationAccepted: false, vmafP5: 99, worstTileSsim: 0.99, latencyP95Ms: 80, wireBitrateMbps: 2 },
  ];
  assert.deepEqual(chooseOptimizationFinalists(candidates, 2).map(item => item.id), ["slow", "fast"]);
});

test("aggressive live optimizer declares the complete resumable experiment matrix", () => {
  const contract = aggressiveOptimizationContract();
  assert.equal(contract.baseline.length, 15);
  assert.equal(contract.screen.length, 192);
  assert.equal(contract.refinementBaseline.length, 6);
  assert.equal(contract.refinement.length, 32);
  assert.deepEqual(contract.validationBandwidths, [2, 4, 8]);
  assert.deepEqual(contract.abbaBandwidths, [2, 4, 8, 12]);
  assert.deepEqual(contract.abbaOrder, ["production", "candidate", "candidate", "production"]);
  assert.equal(new Set([...contract.baseline, ...contract.screen,
    ...contract.refinementBaseline, ...contract.refinement]).size, 245);
});

test("quality capture fails closed unless source and decoded identities match exactly", () => {
  const source = { scene: "motion-60", seed: 469, generation: 7, frameId: 65_537, epochMs: 4_294_967_300 };
  const diagnostics = {
    glassToGlass: { sequence: 1, timestamp: 4, ageMs: 42 },
    renderedTimestamp: 912.5,
    frameTrace: {
      sequence: 91,
      encodedBytes: 65_432,
      key: true,
      recovered: false,
      firstShardAtMs: 800,
      firstShardEventAtMs: 797,
      firstShardHandlerAtMs: 800,
      firstShardQueueDelayMs: 3,
      reconstructingShardAtMs: 820,
      completingShardEventAtMs: 815,
      completingShardHandlerAtMs: 820,
      completingShardQueueDelayMs: 5,
      reconstructedAtMs: 830,
      decodeQueuedAtMs: 850,
      decodedAtMs: 880,
      presentedAtMs: 900,
    },
    benchStatus: { metrics: { encodedBytes: 12_345, encodeUsMax: 1_700 } },
  };
  const aligned = exactFrameAlignment(source, diagnostics, "quality");
  assert.equal(aligned.identity.frameId, 65_537);
  assert.equal(aligned.identity.sequence, 1);
  assert.equal(aligned.identity.timestamp, 4);
  assert.equal(aligned.trace.phase, "quality");
  assert.equal(aligned.trace.encode.encodedBytes, 65_432);
  assert.equal(aligned.trace.encode.key, true);
  assert.equal(aligned.trace.network.recovered, false);
  assert.equal(aligned.trace.network.sequence, 91);
  assert.equal(aligned.trace.network.firstShardAtMs, 800);
  assert.equal(aligned.trace.network.firstShardEventAtMs, 797);
  assert.equal(aligned.trace.network.firstShardQueueDelayMs, 3);
  assert.equal(aligned.trace.network.reconstructingShardAtMs, 820);
  assert.equal(aligned.trace.network.completingShardEventAtMs, 815);
  assert.equal(aligned.trace.network.completingShardQueueDelayMs, 5);
  assert.equal(aligned.trace.decode.queuedAtMs, 850);
  assert.equal(aligned.trace.presentation.presentedAtMs, 900);
  assert.throws(() => exactFrameAlignment(source, diagnostics, "quality", { afterNetworkSequence: 91 }),
    error => error?.code === "quality-frame-before-request");
  const postRequest = exactFrameAlignment(source, {
    ...diagnostics,
    frameTrace: { ...diagnostics.frameTrace, sequence: 92 },
  }, "quality", { afterNetworkSequence: 91 });
  assert.equal(postRequest.trace.network.sequence, 92);
  assert.throws(() => exactFrameAlignment(source, diagnostics, "quality", { exactNetworkSequence: 92 }),
    error => error?.code === "quality-frame-sequence-mismatch");
  const exactRequested = exactFrameAlignment(source, diagnostics, "quality", { exactNetworkSequence: 91 });
  assert.equal(exactRequested.trace.network.sequence, 91);
  const acknowledgedPresented = exactFrameAlignment(source, {
    ...diagnostics,
    frameTrace: { ...diagnostics.frameTrace, sequence: 95, key: false },
    frameTraceHistory: [
      { ...diagnostics.frameTrace, sequence: 91, key: true },
      { ...diagnostics.frameTrace, sequence: 94, key: false },
    ],
  }, "quality", { presentedNetworkSequence: 91 });
  assert.equal(acknowledgedPresented.trace.network.sequence, 91);
  assert.equal(acknowledgedPresented.trace.encode.key, true);
  assert.throws(() => exactFrameAlignment(source, {
    ...diagnostics,
    frameTrace: { ...diagnostics.frameTrace, sequence: 95, key: false },
    frameTraceHistory: [{ ...diagnostics.frameTrace, sequence: 90, key: true }],
  }, "quality", { presentedNetworkSequence: 91 }),
  error => error?.code === "quality-frame-not-presented");
  assert.throws(() => exactFrameAlignment(source, { ...diagnostics, glassToGlass: { sequence: 0, timestamp: 4 } }, "quality"),
    error => error?.code === "frame-identity-mismatch");
  assert.throws(() => exactFrameAlignment(source, { ...diagnostics, glassToGlass: null }, "quality"),
    error => error?.code === "frame-identity-unavailable");
});

test("delta ROI quality alignment requires a mapped non-IDR frame with sparse exact-sequence coverage", () => {
  const source = { scene: "localized-change", seed: 469, generation: 7, frameId: 61, epochMs: 4_294_967_300 };
  const diagnostics = {
    glassToGlass: { sequence: 61, timestamp: 4, ageMs: 42 },
    frameTrace: {
      sequence: 91,
      encodedBytes: 18_000,
      key: false,
      recovered: false,
      roiReason: "applied",
      roiChangedCtbs: 9,
      roiTotalCtbs: 2_040,
    },
  };
  const aligned = exactFrameAlignment(source, diagnostics, "quality-delta", {
    afterNetworkSequence: 90,
    requireDeltaFrame: true,
    requireRoiApplied: true,
  });
  assert.equal(aligned.trace.encode.key, false);
  assert.deepEqual(aligned.roiActivity, {
    sequence: 91,
    reason: "applied",
    changedCtbs: 9,
    totalCtbs: 2_040,
    coverage: 9 / 2_040,
  });
  assert.throws(() => exactFrameAlignment(source, {
    ...diagnostics,
    frameTrace: { ...diagnostics.frameTrace, key: true },
  }, "quality-delta", { requireDeltaFrame: true, requireRoiApplied: true }), /non-IDR/);
  assert.throws(() => exactFrameAlignment(source, {
    ...diagnostics,
    frameTrace: { ...diagnostics.frameTrace, roiReason: "empty-damage", roiChangedCtbs: 0 },
  }, "quality-delta", { requireDeltaFrame: true, requireRoiApplied: true }), /QP map/);
  assert.throws(() => exactFrameAlignment(source, {
    ...diagnostics,
    frameTrace: { ...diagnostics.frameTrace, roiChangedCtbs: 1_020 },
  }, "quality-delta", { requireDeltaFrame: true, requireRoiApplied: true }), /coverage/);
});

test("exact delta alignment survives a later same-identity recovery IDR", () => {
  const source = {
    scene: "scrolling",
    seed: 469,
    generation: 15,
    frameId: 32,
    epochMs: 1_788_058_980_802,
  };
  const presentedIdentity = {
    sequence: source.frameId & 0xffff,
    timestamp: source.epochMs >>> 0,
  };
  const diagnostics = {
    glassToGlass: { ...presentedIdentity, ageMs: 120 },
    frameTrace: {
      sequence: 846,
      key: true,
      presentedIdentity,
      presentedAtMs: 138_367.4,
    },
    frameTraceHistory: [
      {
        sequence: 843,
        key: false,
        presentedIdentity: { sequence: 31, timestamp: (source.epochMs - 100) >>> 0 },
        presentedAtMs: 134_509.6,
      },
      {
        sequence: 844,
        key: false,
        presentedIdentity,
        presentedAtMs: 134_636.4,
      },
      {
        sequence: 845,
        key: true,
        presentedIdentity,
        presentedAtMs: 136_324.2,
      },
    ],
  };

  const aligned = exactFrameAlignment(source, diagnostics, "quality-delta-trigger", {
    afterNetworkSequence: 843,
    requireDeltaFrame: true,
  });
  assert.equal(aligned.trace.network.sequence, 844);
  assert.equal(aligned.trace.encode.key, false);

  assert.throws(() => exactFrameAlignment(source, diagnostics, "quality-capture", {
    afterNetworkSequence: 843,
    requireDeltaFrame: true,
    allowHistoricalDelta: false,
  }), error => error?.code === "quality-frame-not-delta");

  assert.throws(() => exactFrameAlignment(source, {
    ...diagnostics,
    frameTraceHistory: diagnostics.frameTraceHistory.map(trace =>
      trace.sequence === 844
        ? { ...trace, presentedIdentity: { sequence: 31, timestamp: presentedIdentity.timestamp } }
        : trace),
  }, "quality-delta-trigger", {
    afterNetworkSequence: 843,
    requireDeltaFrame: true,
  }), error => error?.code === "quality-frame-not-delta");
});

test("hybrid quality alignment requires an exact frame with a generation-bound tile composite", () => {
  const source = { scene: "source-code", seed: 469, generation: 7, frameId: 61, epochMs: 4_294_967_300 };
  const diagnostics = {
    glassToGlass: { sequence: 61, timestamp: 4, ageMs: 42 },
    frameTrace: {
      sequence: 91,
      encodedBytes: 18_000,
      key: false,
      recovered: false,
      hybridCropRevision: 3,
      hybridTilesDrawn: 1,
    },
  };
  const aligned = exactFrameAlignment(source, diagnostics, "quality-delta", {
    afterNetworkSequence: 90,
    requireDeltaFrame: true,
    requireHybridTiles: true,
  });
  assert.deepEqual(aligned.hybridActivity, {
    sequence: 91,
    cropRevision: 3,
    tilesDrawn: 1,
  });
  assert.throws(() => exactFrameAlignment(source, {
    ...diagnostics,
    frameTrace: { ...diagnostics.frameTrace, hybridTilesDrawn: 0 },
  }, "quality-delta", { requireDeltaFrame: true, requireHybridTiles: true }),
  error => error?.code === "quality-hybrid-tile-missing");
});

test("DDA-gated control quality accepts a stale-free base-only exact frame", () => {
  const source = { scene: "ui-text", seed: 469, generation: 7, frameId: 9, epochMs: 4_294_967_300 };
  const diagnostics = {
    glassToGlass: { sequence: 9, timestamp: 4_294_967_300 >>> 0, ageMs: 42 },
    frameTrace: {
      sequence: 91,
      encodedBytes: 18_000,
      key: false,
      recovered: false,
      hybridCropRevision: 3,
      hybridTilesDrawn: 0,
    },
    hybrid: { staleComposites: 0 },
  };
  const aligned = exactFrameAlignment(source, diagnostics, "quality-delta", {
    requireDeltaFrame: true,
    hybridRequirement: "safe",
  });
  assert.deepEqual(aligned.hybridActivity, {
    sequence: 91,
    cropRevision: 3,
    tilesDrawn: 0,
    requirement: "safe",
  });
  assert.throws(() => exactFrameAlignment(source, {
    ...diagnostics,
    hybrid: { staleComposites: 1 },
  }, "quality-delta", { requireDeltaFrame: true, hybridRequirement: "safe" }),
  error => error?.code === "quality-hybrid-stale-composite");
});

test("quiescent hybrid motion samples require a safe base-only bypass", () => {
  const source = { scene: "motion-60", seed: 469, generation: 7, frameId: 61, epochMs: 4_294_967_300 };
  const diagnostics = {
    glassToGlass: { sequence: 61, timestamp: 4_294_967_300 >>> 0, ageMs: 42 },
    frameTrace: {
      sequence: 91,
      encodedBytes: 18_000,
      key: false,
      recovered: false,
      hybridCropRevision: 3,
      hybridTilesDrawn: 0,
    },
    hybrid: { staleComposites: 0 },
  };
  const aligned = exactFrameAlignment(source, diagnostics, "quality-delta", {
    requireDeltaFrame: true,
    hybridRequirement: "guard",
  });
  assert.deepEqual(aligned.hybridActivity, {
    sequence: 91,
    cropRevision: 3,
    tilesDrawn: 0,
    requirement: "guard",
  });
  assert.throws(() => exactFrameAlignment(source, {
    ...diagnostics,
    frameTrace: { ...diagnostics.frameTrace, hybridTilesDrawn: 1 },
  }, "quality-delta", { requireDeltaFrame: true, hybridRequirement: "guard" }),
  error => error?.code === "quality-hybrid-motion-overlay");
  assert.throws(() => exactFrameAlignment(source, {
    ...diagnostics,
    hybrid: { staleComposites: 1 },
  }, "quality-delta", { requireDeltaFrame: true, hybridRequirement: "guard" }),
  error => error?.code === "quality-hybrid-stale-composite");
});

test("performance tracing preserves receiver stages when the independently sampled source frame differs", () => {
  const source = { scene: "motion-60", seed: 469, generation: 7, frameId: 92, epochMs: 4_294_967_300 };
  const diagnostics = {
    glassToGlass: { sequence: 91, timestamp: 4_294_967_299, ageMs: 142 },
    clock: { performanceMs: 900, epochMs: 1_700_000_000_000 },
    frameTrace: {
      sequence: 451,
      encodedBytes: 12_345,
      key: false,
      recovered: true,
      firstShardHostSentAtEpochMs: 1_699_999_999_868,
      firstShardEdgeAtEpochMs: 1_699_999_999_870,
      firstShardAtMs: 800,
      reconstructingShardAtMs: 824,
      reconstructedAtMs: 824.2,
      decodeQueuedAtMs: 824.4,
      decodedAtMs: 830,
      presentedAtMs: 832,
    },
  };

  const trace = performanceFrameTrace(source, diagnostics);
  assert.equal(trace.identity, null);
  assert.equal(trace.trace.phase, "performance-unmatched");
  assert.equal(trace.trace.network.sequence, 451);
  assert.equal(trace.trace.network.recovered, true);
  assert.equal(trace.trace.network.firstShardAtMs, 800);
  assert.equal(trace.trace.network.sourceToHostEgressMs, 10);
  assert.equal(trace.trace.network.hostEgressToEdgeMs, 2);
  assert.equal(trace.trace.network.hostEgressToFirstShardMs, 32);
  assert.equal(trace.trace.network.sourceToEdgeMs, 12);
  assert.equal(trace.trace.network.edgeToFirstShardMs, 30);
  assert.equal(trace.trace.network.sourceToFirstShardMs, 42);
  assert.equal(trace.trace.network.reconstructedAtMs, 824.2);
  assert.equal(trace.trace.decode.decodedAtMs, 830);
  assert.equal(trace.trace.presentation.presentedAtMs, 832);
  assert.equal(trace.trace.presentation.sourceToPresentedMs, 74);
});

test("temporal capture plans are deterministic and bounded", () => {
  const plan = buildTemporalCapturePlan(["ui-text", "motion-60", "localized-change", "generated-video"], { maxSamples: 7 });
  assert.equal(plan.length, 7);
  assert.deepEqual(plan[0], { scene: "ui-text", frameId: 0, workload: "synthetic" });
  assert.ok(plan.some(sample => sample.scene === "motion-60" && sample.frameId > 0));
  assert.ok(plan.some(sample => sample.scene === "localized-change" && sample.frameId === 90));
  assert.deepEqual(plan, buildTemporalCapturePlan(["ui-text", "motion-60", "localized-change", "generated-video"], { maxSamples: 7 }));
  assert.throws(() => buildTemporalCapturePlan(["motion-60"], { maxSamples: 0 }), /maxSamples/);
});

test("delta temporal capture plans anchor immediately before localized phase changes", () => {
  const plan = buildTemporalCapturePlan(["localized-change"], { maxSamples: 2, deltaMode: true });
  assert.deepEqual(plan, [
    { scene: "localized-change", frameId: 59, workload: "synthetic" },
    { scene: "localized-change", frameId: 89, workload: "synthetic" },
  ]);
});

test("hybrid workload planning uses only an allowlisted tracked source and deterministic local fallbacks", () => {
  const plan = buildHybridWorkloadPlan({
    repoRoot: "C:\\repo", vscodePath: "C:\\Code.exe",
    trackedFiles: ["worker/public/index.html", ".env", "README.md", "host/src/main.rs"],
  });
  assert.deepEqual(plan.map(item => item.id), ["vscode-source", "source-code-fixture", "chrome-text", "chrome-canvas", "generated-video"]);
  assert.equal(plan[0].isolatedProfile, true);
  assert.deepEqual(plan[0].allowlistedFiles, ["worker/public/index.html", "host/src/main.rs"]);
  assert.equal(plan[0].availability, "unsupported");
  assert.equal(plan[0].coverageEligible, false);
  assert.equal(plan[1].exactAlignment, true);
  assert.equal(plan[1].coverageEligible, true);
  assert.ok(plan.every(item => item.privateContent === false));
});

test("only ten worst source-decoded pairs are retained and routine artifacts are removed", () => {
  const root = mkdtempSync(join(tmpdir(), "bubbleplay-pairs-"));
  const samples = Array.from({ length: 12 }, (_, index) => {
    const paths = ["reference.png", "decoded.png", "metric.json"].map(name => join(root, `${index}-${name}`));
    for (const path of paths) writeFileSync(path, String(index));
    return { scene: "motion-60", frameId: index, vmaf: 100 - index, ssim: 1 - index / 100,
      referencePath: paths[0], decodedPath: paths[1], artifactPaths: paths };
  });
  const retained = retainWorstSamplePairs(samples, { limit: 10 });
  assert.deepEqual(retained.filter(sample => sample.retained).map(sample => sample.frameId), [2,3,4,5,6,7,8,9,10,11]);
  assert.equal(existsSync(samples[0].referencePath), false);
  assert.equal(existsSync(samples[1].decodedPath), false);
  assert.equal(existsSync(samples[11].referencePath), true);
  assert.equal(retained[0].referencePath, null);
  assert.equal(retained[0].artifactPaths.length, 0);
});

test("lab Chrome closes gracefully before force-kill so remembered cookies persist", async () => {
  const child = new EventEmitter();
  child.pid = 469;
  child.exitCode = null;
  const forced = [];
  const page = { call: async method => {
    assert.equal(method, "Browser.close");
    queueMicrotask(() => { child.exitCode = 0; child.emit("exit", 0); });
  } };
  const result = await shutdownChrome(page, child, {
    forceKill: pid => forced.push(pid), closeTimeoutMs: 20, exitTimeoutMs: 20,
  });
  assert.equal(result, "graceful");
  assert.deepEqual(forced, []);
});

test("every live-unit health check consults the campaign activity guard before the helper lease", () => {
  const events = [];
  const session = new LiveStreamLabSession({ runId: "123e4567-e89b-42d3-a456-426614174000",
    store: { appendTelemetry() {} }, assertCampaignActive: () => events.push("activity") });
  session.heartbeat = { assertHealthy: () => events.push("lease") };
  session.assertLeaseHealthy();
  assert.deepEqual(events, ["activity", "lease"]);
  const aborted = new LiveStreamLabSession({ runId: "123e4567-e89b-42d3-a456-426614174000",
    store: { appendTelemetry() {} }, assertCampaignActive: () => { throw new Error("physical activity"); } });
  aborted.heartbeat = { assertHealthy: () => events.push("must-not-run") };
  assert.throws(() => aborted.assertLeaseHealthy(), /physical activity/);
  assert.doesNotMatch(events.join(","), /must-not-run/);
});

test("transient display selection requires one new 1080p screen", () => {
  const before = [{ deviceName: "\\\\.\\DISPLAY1", x: 0, y: 0, width: 1920, height: 1080, primary: true }];
  const virtual = { deviceName: "\\\\.\\DISPLAY2", x: 1920, y: 0, width: 1920, height: 1080, primary: false };
  assert.deepEqual(selectTransientDisplay(before, [...before, virtual]), virtual);
  assert.throws(() => selectTransientDisplay(before, before), /did not create/);
  assert.throws(() => selectTransientDisplay(before, [...before, { ...virtual, width: 1280 }]), /1920×1080/);
});

test("an explicit transient-display session asks Lab Helper to own the VDD lease", async (t) => {
  const root = mkdtempSync(join(tmpdir(), "bubbleplay-lab-transient-vdd-"));
  const requests = [];
  const session = new LiveStreamLabSession({
    runId: "123e4567-e89b-42d3-a456-426614174000",
    store: { appendTelemetry() {} },
    env: { LOCALAPPDATA: root },
    chrome: process.execPath,
    transientVdd: true,
    helperSessionFactory: async request => {
      requests.push(request);
      throw new Error("stop after helper boundary");
    },
  });
  t.after(() => {
    session.disposeTermination?.();
    rmSync(root, { recursive: true, force: true });
  });
  session.startReceiver = async () => {};
  session.admitCompatibility = async () => {};

  await assert.rejects(() => session.start("wss"), /stop after helper boundary/);
  assert.equal(requests.length, 1);
  assert.equal(requests[0].vdd, true);
});

test("source window coverage is evaluated against the transient display, not the primary", () => {
  const display = { x: 3840, y: 0, width: 1920, height: 1080 };
  assert.equal(windowCoversDisplay({ left: 3840, top: 0, width: 1920, height: 1080 }, display), true);
  assert.equal(windowCoversDisplay({ left: 0, top: 0, width: 1920, height: 1080 }, display), false);
  assert.equal(windowCoversDisplay({ left: 3840, top: 0, width: 800, height: 600 }, display), false);
});

test("source placement leaves stale fullscreen state before targeting a recreated VDD", async () => {
  const display = { x: 3840, y: 0, width: 1920, height: 1080 };
  const calls = [];
  let bounds = { left: 1920, top: 0, width: 1920, height: 1080, windowState: "fullscreen" };
  const page = {
    async call(method, payload) {
      calls.push([method, payload]);
      if (method === "Browser.setWindowBounds") {
        if (payload.bounds.windowState === "normal") bounds = { ...bounds, windowState: "normal" };
        else if (payload.bounds.windowState === "fullscreen") bounds = { ...bounds, windowState: "fullscreen" };
        else bounds = { ...bounds, ...payload.bounds };
      }
      if (method === "Browser.getWindowForTarget") return { windowId: 7, bounds };
      return {};
    },
  };
  const placed = await placeSourceWindow(page, 123, display, {
    sleep: async () => {},
    nativePlace: (_pid, target) => { calls.push(["native", target]); bounds = { ...bounds,
      left: target.x, top: target.y, width: target.width, height: target.height }; },
  });
  assert.equal(windowCoversDisplay(placed.bounds, display), true);
  assert.deepEqual(calls.slice(0, 4).map(call => call[0]), [
    "Browser.getWindowForTarget", "Browser.setWindowBounds", "Browser.setWindowBounds", "native",
  ]);
  assert.ok(calls.findIndex(call => call[0] === "native") <
    calls.findIndex(call => call[1]?.bounds?.windowState === "fullscreen"));
});

test("receiver URL fixes Work profile and forces relay only for WebRTC", () => {
  const wss = buildReceiverUrl("https://play.example/", "wss");
  assert.equal(wss.searchParams.get("transport"), "wss");
  assert.equal(wss.searchParams.get("quality"), "high");
  assert.equal(wss.searchParams.get("adaptive"), "0");
  assert.equal(wss.searchParams.has("ice"), false);
  const rtc = buildReceiverUrl("https://play.example/", "webrtc");
  assert.equal(rtc.searchParams.get("ice"), "relay");
  const adaptiveDirect = buildReceiverUrl("https://play.example/", "webrtc", { relay: false, adaptive: true });
  assert.equal(adaptiveDirect.searchParams.has("ice"), false);
  assert.equal(adaptiveDirect.searchParams.get("adaptive"), "1");
  assert.equal(adaptiveDirect.searchParams.get("quality"), "auto");
  const labRun = "123e4567-e89b-42d3-a456-426614174000";
  const isolated = buildReceiverUrl("https://play.example/", "wss", { labRun });
  assert.equal(isolated.searchParams.get("labRun"), labRun);
});

test("receiver URL defaults to the diagnostic decoder and carries a native opt-in", () => {
  const diagnostic = buildReceiverUrl("https://play.example/", "webrtc", { relay: false });
  assert.equal(diagnostic.searchParams.get("decoderPath"), "webcodecs");
  const native = buildReceiverUrl("https://play.example/", "webrtc", { relay: false, decoderPath: "native" });
  assert.equal(native.searchParams.get("decoderPath"), "native");
  // An unknown decoder path never silently selects native playback.
  const invalid = buildReceiverUrl("https://play.example/", "webrtc", { decoderPath: "hardware" });
  assert.equal(invalid.searchParams.get("decoderPath"), "webcodecs");
});

test("session receiver options keep the decoder path across partial transport switches", () => {
  const current = { relay: false, adaptive: false, decoderPath: "native" };
  assert.equal(normalizeDecoderPath(undefined), "webcodecs");
  assert.equal(normalizeDecoderPath("native"), "native");
  // A partial switch inherits the session decoder path; an explicit one wins.
  assert.equal(normalizeReceiverOptions({ adaptive: false }, current).decoderPath, "native");
  assert.equal(normalizeReceiverOptions({ decoderPath: "webcodecs" }, current).decoderPath, "webcodecs");
  assert.equal(normalizeReceiverOptions({}, {}).decoderPath, "webcodecs");
  // A decoder-path-only switch must not be treated as a no-op.
  assert.equal(sameReceiverOptions(normalizeReceiverOptions({ relay: false, adaptive: false }, current), current), true);
  assert.equal(sameReceiverOptions(normalizeReceiverOptions({ decoderPath: "webcodecs" }, current), current), false);
  assert.equal(sameReceiverOptions(normalizeReceiverOptions({ relay: true }, current), current), false);
});

test("receiver URL carries only validated protocol-v3 topology controls", () => {
  const labRun = "123e4567-e89b-42d3-a456-426614174000";
  const url = buildReceiverUrl("https://play.example/", "wss", {
    labRun, wssTopology: { laneCount: 9, dataShards: 8, parityShards: 1 },
  });
  assert.equal(url.searchParams.get("wssLanes"), "9");
  assert.equal(url.searchParams.get("wssParity"), "1");
  const duplicateParity = buildReceiverUrl("https://play.example/", "wss", {
    labRun, wssTopology: { laneCount: 16, dataShards: 14, parityShards: 2 },
  });
  assert.equal(duplicateParity.searchParams.get("wssLanes"), "16");
  assert.equal(duplicateParity.searchParams.get("wssParity"), "2");
  assert.throws(() => buildReceiverUrl("https://play.example/", "wss", {
    labRun, wssTopology: { laneCount: 17, dataShards: 16, parityShards: 1 },
  }), /topology/i);
});

test("candidate host launch reuses the broker-owned VDD and reads only its scratch profile", () => {
  const runId = "123e4567-e89b-42d3-a456-426614174000";
  const profilePath = "C:\\lab\\candidate-profile.json";
  assert.equal(buildLabHostUrl("https://play.example/something", runId).href,
    `wss://play.example/lab-host-ws?run=${runId}&compat=wss-v3-host20-edge12-bench2-hevc-tiles-bpip4-v15`);
  const launch = buildCandidateHostLaunch("C:\\lab\\bubbleplay-host.exe", "https://play.example/", runId, profilePath);
  assert.equal(launch.command, "C:\\lab\\bubbleplay-host.exe");
  assert.deepEqual(launch.args.slice(0, 4), ["--mode", "serve", "--worker",
    `wss://play.example/lab-host-ws?run=${runId}&compat=wss-v3-host20-edge12-bench2-hevc-tiles-bpip4-v15`]);
  assert.deepEqual(launch.args.slice(-2), ["--benchmark-profile", profilePath]);
  assert.deepEqual(launch.env, {
    BUBBLEPLAY_LAB_IDR_TELEMETRY: "1",
    BUBBLEPLAY_LAB_RESET_ENCODER_ON_DIAGNOSTIC_KEYFRAME: "1",
  });
  assert.equal(launch.args.includes("--privileged-agent"), false);
  assert.equal(launch.args.includes("--vdd"), false);

  const steadyStateLaunch = buildCandidateHostLaunch(
    "C:\\lab\\bubbleplay-host.exe",
    "https://play.example/",
    runId,
    profilePath,
    { resetEncoderOnQualityKeyframe: false },
  );
  assert.equal(
    steadyStateLaunch.env.BUBBLEPLAY_LAB_RESET_ENCODER_ON_DIAGNOSTIC_KEYFRAME,
    "0",
  );

  const versionedLaunch = buildCandidateHostLaunch(
    "C:\\lab\\bubbleplay-host.exe",
    "https://play.example/",
    runId,
    profilePath,
    {
      workerVersionOverride: {
        workerName: "playfull-cat",
        versionId: "c9d1446b-5314-468c-bfad-12595f9a0090",
      },
    },
  );
  assert.equal(versionedLaunch.env.BUBBLEPLAY_LAB_WORKER_NAME, "playfull-cat");
  assert.equal(
    versionedLaunch.env.BUBBLEPLAY_LAB_WORKER_VERSION_ID,
    "c9d1446b-5314-468c-bfad-12595f9a0090",
  );
});

test("persistent lab display is resolved from the host's Parsec monitor identity", () => {
  const displays = [
    { deviceName: "\\\\.\\DISPLAY1", x: 0, y: 0, width: 1920, height: 1080, primary: true },
    { deviceName: "\\\\.\\DISPLAY2", x: 1920, y: 0, width: 1920, height: 1080, primary: false },
    { deviceName: "\\\\.\\DISPLAY5", x: 3840, y: 0, width: 1920, height: 1080, primary: false },
  ];
  const monitors = [
    { selector: "\\\\.\\DISPLAY1", name: "S2419HGF", width: 1920, height: 1080 },
    { selector: "\\\\.\\DISPLAY2", name: "L22e-30", width: 1920, height: 1080 },
    { selector: "\\\\.\\DISPLAY5", name: "ParsecVDA", width: 1920, height: 1080 },
  ];
  assert.deepEqual(selectPersistentVirtualDisplay(displays, monitors), displays[2]);
  assert.throws(() => selectPersistentVirtualDisplay(displays.slice(0, 2), monitors), /not attached/i);
});

test("topology parity trials alternate order and repeat every candidate", () => {
  const topologies = [null,
    { laneCount: 5, dataShards: 4, parityShards: 1 },
    { laneCount: 9, dataShards: 8, parityShards: 1 },
  ];
  const trials = buildTopologyTrialPlan(topologies, 3);
  assert.equal(trials.length, 9);
  assert.deepEqual(trials.slice(0, 3).map(trial => trial.id), ["v2-3+1", "v3-4+1", "v3-8+1"]);
  assert.deepEqual(trials.slice(3, 6).map(trial => trial.id), ["v2-3+1", "v3-8+1", "v3-4+1"]);
  assert.deepEqual(trials.slice(6).map(trial => trial.id), ["v2-3+1", "v3-4+1", "v3-8+1"]);
  assert.throws(() => buildTopologyTrialPlan(topologies, 0), /repetitions/);
});

test("corrected-score quality topology frontier retains the measured 5+1, 8+1, and 15+1 comparison", () => {
  assert.deepEqual(defaultTopologyFrontier({ quality: true }), [
    { laneCount: 6, dataShards: 5, parityShards: 1 },
    { laneCount: 9, dataShards: 8, parityShards: 1 },
    { laneCount: 16, dataShards: 15, parityShards: 1 },
  ]);
});

test("live sessions expose a bounded transport restart for topology first-frame recovery", () => {
  assert.equal(typeof LiveStreamLabSession.prototype.restartTransport, "function");
});

test("topology apply retries one first-frame timeout and no other failure", async () => {
  const events = [];
  const session = {
    receiverOptions: { wssTopology: { laneCount: 5, dataShards: 4, parityShards: 1 } },
    store: { appendTelemetry: event => events.push(event.phase) },
    async apply() {
      events.push("apply");
      if (events.filter(event => event === "apply").length === 1)
        throw Object.assign(new Error("startup"), { code: "first-frame-timeout" });
      return "ready";
    },
    async restartTransport() { events.push("restart"); },
  };
  assert.equal(await applyWithFirstFrameRetry(session, { id: "candidate" }, 4), "ready");
  assert.deepEqual(events, ["apply", "topology-first-frame-retry", "restart", "apply"]);
  const broken = { ...session, async apply() { throw Object.assign(new Error("decode"), { code: "decoder-failed" }); } };
  await assert.rejects(applyWithFirstFrameRetry(broken, { id: "candidate" }, 4), /decode/);
  const source = readFileSync(new URL("../tools/stream-lab-live.mjs", import.meta.url), "utf8");
  assert.match(source, /runLiveTopologySweep[\s\S]*?applyWithFirstFrameRetry\(session, effective, bandwidthMbps\)/);
});

test("receiver Chrome starts minimized until it can be moved behind the lab source", () => {
  const args = buildReceiverChromeArgs("C:\\lab-profile", 19341, "https://play.example/");
  assert.ok(args.includes("--start-minimized"));
  assert.ok(args.includes("--disable-renderer-backgrounding"));
});

test("a pinned local receiver attaches CDP before its first target navigation", () => {
  const target = "https://play.example/?transport=wss&labRun=123e4567-e89b-42d3-a456-426614174000";
  assert.equal(liveModule.receiverBootstrapUrl(target, {
    workerVersionOverride: {
      workerName: "playfull-cat",
      versionId: "a34966a1-d238-4dec-8dbe-5405f6a9fdce",
    },
    receiverAssetOverride: true,
  }), "about:blank");
  assert.equal(liveModule.receiverBootstrapUrl(target, {
    workerVersionOverride: null,
    receiverAssetOverride: false,
  }), target);
});

test("loopback receiver disables Chrome WebSocket local-network prompting", () => {
  const loopback = buildReceiverChromeArgs("C:\\lab-profile", 19341, "http://127.0.0.1:8787/");
  const remote = buildReceiverChromeArgs("C:\\lab-profile", 19341, "https://play.example/");

  assert.ok(loopback.includes("--disable-features=LocalNetworkAccessChecksWebSockets"));
  assert.equal(remote.includes("--disable-features=LocalNetworkAccessChecksWebSockets"), false);
});

test("manual-use startup cannot relocate the receiver or launch interaction automation", () => {
  assert.equal(typeof liveModule.sessionStartupPlan, "function");
  assert.deepEqual(liveModule.sessionStartupPlan("manual"), {
    receiverStartMinimized: false,
    placeReceiverOnCaptureDisplay: false,
    startSyntheticSource: false,
  });
  assert.deepEqual(liveModule.sessionStartupPlan("benchmark"), {
    receiverStartMinimized: true,
    placeReceiverOnCaptureDisplay: true,
    startSyntheticSource: true,
  });
  const args = buildReceiverChromeArgs(
    "C:\\lab-profile", 19341, "https://play.example/", { startMinimized: false },
  );
  assert.equal(args.includes("--start-minimized"), false);
});

test("manual-use runner applies the candidate without entering benchmark interaction phases", async () => {
  const events = [];
  const candidate = {
    id: "hybrid-png-quiescent-detail",
    experimentId: "hybrid-png-quiescent-detail",
    hybridMode: "png-quiescent-detail",
  };
  const session = {
    async start(transport, options) { events.push(["start", transport, options]); },
    async applyForManualUse(value, bandwidth, transport) {
      events.push(["apply-manual", value.id, bandwidth, transport]);
      return { route: "wss", dec: "hevc" };
    },
    async stop() { events.push(["stop"]); },
  };
  let report;
  await liveModule.runLiveManual({
    runId: "123e4567-e89b-42d3-a456-426614174000",
    candidate,
    session,
    store: { writeMetrics(value) { report = value; } },
    waitUntilStopped: async () => { events.push(["hold-open"]); },
  });
  assert.deepEqual(events, [
    ["start", "wss", { mode: "manual" }],
    ["apply-manual", candidate.id, null, "wss"],
    ["hold-open"],
    ["stop"],
  ]);
  assert.equal(report.interactionAutomation, false);
  assert.equal(report.result, "running");
});

test("candidate profile lease is local, expiring, and revisioned without helper-owned production state", () => {
  const runId = "123e4567-e89b-42d3-a456-426614174000";
  const profile = benchmarkProfile({ schemaVersion: 2, implementationVariant: "candidate", experimentId: "aq-p4",
    preset: "p4", tuning: "ultra-low-latency", multipass: "quarter", aqMode: "spatial", aqStrength: 8,
    vbvFrames: 2, lookaheadFrames: 1, weightedPrediction: false, bitrateMbps: 4, fps: 30, gopSeconds: 2 }, "wss");
  const lease = buildCandidateProfileLease(profile, runId, 1_700_000_000_000, 7);
  assert.equal(lease.runId, runId);
  assert.equal(lease.expiresAtMs, 1_700_000_600_000);
  assert.equal(lease.applyRevision, 7);
  assert.equal(lease.implementationVariant, "candidate");
  assert.equal(lease.bitrateBps, 4_000_000);
});

test("encoder screening is unshaped while constrained runs reserve transport headroom", () => {
  assert.equal(effectiveEncoderBitrateMbps(6, null, "wss"), 6);
  assert.equal(effectiveEncoderBitrateMbps(6, 4, "wss"), 2.6);
  assert.equal(effectiveEncoderBitrateMbps(8, 8, "webrtc"), 7.2);
  const source = readFileSync(new URL("../tools/stream-lab-live.mjs", import.meta.url), "utf8");
  assert.match(source, /runSweepCandidate\(session, candidate, options, null\)/);
  assert.match(source, /options\.bandwidthMbps === undefined \? null/);
});

test("host monitor selection is exact when possible and otherwise unambiguous", () => {
  const display = { deviceName: "\\\\.\\DISPLAY2", width: 1920, height: 1080 };
  const exact = selectHostMonitor([
    { selector: "\\\\.\\DISPLAY1", name: "Primary", width: 1920, height: 1080, selected: true },
    { selector: "\\\\.\\DISPLAY2", name: "BubblePlay Virtual Display", width: 1920, height: 1080, selected: false },
  ], display);
  assert.equal(exact.selector, "\\\\.\\DISPLAY2");
  assert.throws(() => selectHostMonitor([
    { selector: "a", name: "Display A", width: 1920, height: 1080, selected: false },
    { selector: "b", name: "Display B", width: 1920, height: 1080, selected: false },
  ], display), /unambiguously/);
});

test("virtual monitor selection is scoped to the current signaling registration", () => {
  const display = { deviceName: "\\\\.\\DISPLAY5", x: 3840, y: 0, width: 1920, height: 1080 };
  const snapshot = { signaling: {
    registered: true,
    registeredAtMs: 2_000,
    monitors: [
      { selector: "\\\\.\\DISPLAY1", name: "Primary", width: 1920, height: 1080, selected: false },
      { selector: "\\\\.\\DISPLAY5", name: "ParsecVDA", width: 1920, height: 1080, selected: true },
    ],
  } };

  assert.equal(virtualMonitorSelectionCurrent(snapshot, display, 2_000), true);
  assert.equal(virtualMonitorSelectionCurrent(snapshot, display, 1_000), false);
  assert.equal(virtualMonitorSelectionCurrent({
    signaling: { ...snapshot.signaling, monitors: snapshot.signaling.monitors.map(monitor => ({
      ...monitor, selected: monitor.selector === "\\\\.\\DISPLAY1",
    })) },
  }, display, 2_000), false);
});

test("virtual monitor selection is reasserted even when the browser cache already marks it selected", async () => {
  const session = Object.create(LiveStreamLabSession.prototype);
  session.display = { deviceName: "\\\\.\\DISPLAY5", x: 3840, y: 0, width: 1920, height: 1080 };
  session.assertLeaseHealthy = () => {};
  let monitorRevision = 4;
  session.snapshot = async () => ({ signaling: { registered: true, monitorRevision, monitors: [
    { selector: "\\\\.\\DISPLAY1", name: "Primary", width: 1920, height: 1080, selected: false },
    { selector: "\\\\.\\DISPLAY5", name: "ParsecVDA", width: 1920, height: 1080, selected: true },
  ] } });
  const commands = [];
  session.receiver = { evaluate: async expression => { commands.push(expression); monitorRevision++; return true; } };
  await session.selectVirtualMonitor();
  assert.equal(commands.filter(command => command.includes("selectMonitor")).length, 1);
  assert.match(commands.find(command => command.includes("selectMonitor")), /DISPLAY5/);
});

test("virtual monitor selection waits for reconnect registration and retries the exact persistent VDD", async () => {
  const session = Object.create(LiveStreamLabSession.prototype);
  session.display = { deviceName: "\\\\.\\DISPLAY5", x: 3840, y: 0, width: 1920, height: 1080 };
  session.assertLeaseHealthy = () => {};
  let snapshotCount = 0;
  let monitorRevision = 7;
  let targetSelected = false;
  const commands = [];
  session.snapshot = async () => {
    snapshotCount++;
    return { signaling: {
      registered: snapshotCount >= 3,
      monitorRevision,
      monitors: [
        { selector: "\\\\.\\DISPLAY1", name: "Primary", width: 1920, height: 1080, selected: !targetSelected },
        { selector: "\\\\.\\DISPLAY5", name: "ParsecVDA", width: 1920, height: 1080, selected: targetSelected },
      ],
    } };
  };
  let selectionAttempts = 0;
  session.receiver = { evaluate: async expression => {
    commands.push(expression);
    if (expression.includes("requestMonitors")) {
      monitorRevision++;
      return true;
    }
    if (expression.includes("selectMonitor")) {
      selectionAttempts++;
      if (selectionAttempts === 1) return false;
      targetSelected = true;
      monitorRevision++;
      return true;
    }
    return false;
  } };
  await session.selectVirtualMonitor();
  const refreshes = commands.filter(command => command.includes("requestMonitors"));
  const selections = commands.filter(command => command.includes("selectMonitor"));
  assert.ok(refreshes.length >= 2);
  assert.equal(selections.length, 2);
  assert.ok(commands.indexOf(refreshes[0]) < commands.indexOf(selections[0]));
  assert.ok(selections.every(command => command.includes("DISPLAY5") && !command.includes("DISPLAY1")));
});

test("monitor selection failure classification separates deferral from host rejection", () => {
  const deferred = { sawRegistered: true, sawFreshMonitorList: true, sawExactTarget: true,
    sawHostRejection: false, sawDeferredSelection: true };
  assert.equal(classifyMonitorSelectionFailure(deferred).code, "monitor-selection-deferred");
  assert.equal(classifyMonitorSelectionFailure({ ...deferred, sawHostRejection: true }).code,
    "monitor-selection-rejected");
  assert.equal(classifyMonitorSelectionFailure({ ...deferred, sawDeferredSelection: false }).code,
    "monitor-switch-timeout");
  assert.equal(classifyMonitorSelectionFailure({ ...deferred, sawRegistered: false }).code,
    "signaling-registration-timeout");
  assert.equal(classifyMonitorSelectionFailure({ ...deferred, sawFreshMonitorList: false }).code,
    "monitor-list-timeout");
  assert.equal(classifyMonitorSelectionFailure({ ...deferred, sawExactTarget: false }).code,
    "monitor-ambiguous");
});

test("benchmark profile maps a candidate into the helper's nested schema", () => {
  assert.deepEqual(benchmarkProfile({
    preset: "p4", tuning: "low-latency", multipass: "quarter", aqStrength: 10,
    temporalAq: true, vbvFrames: 2, bitrateMbps: 4,
  }, "wss"), {
    schemaVersion: 1, transport: "wss", codec: "hevc", streamMode: "work", adaptive: false,
    width: 1920, height: 1080, fps: 30, bitrateBps: 4_000_000, gopSeconds: 2,
    encoder: { preset: "p4", tuning: "low-latency", multipass: "quarter", aqStrength: 10, temporalAq: true, vbvFrames: 2 },
  });
  assert.deepEqual(benchmarkProfile({
    preset: "p3", tuning: "ultra-low-latency", multipass: "disabled", aqStrength: 8,
    temporalAq: false, vbvFrames: 1, bitrateMbps: 8, codec: "hevc", streamMode: "game", fps: 60, gopSeconds: 1,
  }, "webrtc"), {
    schemaVersion: 1, transport: "webrtc", codec: "hevc", streamMode: "game", adaptive: false,
    width: 1920, height: 1080, fps: 60, bitrateBps: 8_000_000, gopSeconds: 1,
    encoder: { preset: "p3", tuning: "ultra-low-latency", multipass: "disabled", aqStrength: 8, temporalAq: false, vbvFrames: 1 },
  });
  assert.throws(() => benchmarkProfile({ codec: "h264" }, "wss"), /release the override/i);
});

test("benchmark profile v2 normalizes pipeline controls and preserves exact apply matching", () => {
  const profile = benchmarkProfile({ schemaVersion: 2, implementationVariant: "candidate", experimentId: "enc-work-001",
    preset: "p7", tuning: "ultra-low-latency", multipass: "full", aqMode: "spatial", aqStrength: 15,
    vbvFrames: 4, lookaheadFrames: 2, weightedPrediction: true, dirtyQpMap: true, main10: false,
    bitrateMbps: 6, gopSeconds: 2,
    wssTopology: { laneCount: 16, dataShards: 15, parityShards: 1 } }, "wss");
  assert.deepEqual(profile, {
    schemaVersion: 2, implementationVariant: "candidate", experimentId: "enc-work-001",
    transport: "wss", codec: "hevc", streamMode: "work", adaptive: false,
    width: 1920, height: 1080, fps: 30, bitrateBps: 6_000_000, gopSeconds: 2,
    wssTopology: { laneCount: 16, dataShards: 15, parityShards: 1 }, hybridMode: "off",
    hybridCacheMode: "suppress-reactivate",
    encoder: { preset: "p7", tuning: "ultra-low-latency", multipass: "full", aqMode: "spatial",
      aqStrength: 15, vbvFrames: 4, lookaheadDepth: 2, weightedPrediction: true, dirtyQpMap: true,
      main10: false, roiMode: "damage", resourceMode: "per-frame", asyncDepth: 0 },
  });
  const { schemaVersion, ...effective } = profile;
  const runId = "123e4567-e89b-42d3-a456-426614174000";
  assert.equal(benchStatusConfirmsApply(null, { protocol: 2, runId, profileRevision: 1, phase: "applied", effective }, profile, runId), true);
  assert.equal(benchStatusConfirmsApply(null, { protocol: 1, runId, profileRevision: 1, phase: "applied", effective }, profile, runId), false);
  assert.throws(() => benchmarkProfile({ schemaVersion: 2, implementationVariant: "candidate", experimentId: "bad id",
    preset: "p3", multipass: "disabled", aqMode: "none", aqStrength: 8, vbvFrames: 1,
    lookaheadFrames: 0, weightedPrediction: false, bitrateMbps: 4, gopSeconds: 2 }), /experimentId/);
  assert.throws(() => benchmarkProfile({ schemaVersion: 2, implementationVariant: "candidate", experimentId: "game-too-deep",
    streamMode: "game", preset: "p3", multipass: "disabled", aqMode: "none", aqStrength: 8, vbvFrames: 1,
    lookaheadFrames: 2, weightedPrediction: false, bitrateMbps: 8, gopSeconds: 1 }), /lookahead/);
  assert.throws(() => benchmarkProfile({ schemaVersion: 2, implementationVariant: "candidate", experimentId: "mixed",
    preset: "p3", multipass: "disabled", aqMode: "spatial", aqStrength: 8, vbvFrames: 1,
    lookaheadFrames: 0, weightedPrediction: false, bitrateMbps: 6, gopSeconds: 2,
    wssTopology: { laneCount: 16, dataShards: 15, parityShards: 1 },
    roiMode: "damage", hybridMode: "png-static-roi" }), /experiment|axis|controls/i);
});

test("benchmark profile v2 preserves the bounded external-budget VBR contract", () => {
  const candidate = {
    schemaVersion: 2, implementationVariant: "candidate", experimentId: "external-budget-vbr-cq23",
    preset: "p3", tuning: "ultra-low-latency", multipass: "disabled", aqMode: "spatial",
    aqStrength: 8, vbvFrames: 4, lookaheadFrames: 0, weightedPrediction: false,
    dirtyQpMap: false, main10: false, roiMode: "off", resourceMode: "per-frame", asyncDepth: 0,
    hybridMode: "off", rateControlMode: "external-budget-vbr", targetQuality: 23,
    peakBitrateMultiplier: 4, requestedBitrateMbps: 6, totalMediaCapMbps: 6,
    wssTopology: { laneCount: 16, dataShards: 15, parityShards: 1 },
  };
  const profile = benchmarkProfile(candidate, "wss");
  assert.equal(profile.encoder.rateControlMode, "external-budget-vbr");
  assert.equal(profile.encoder.targetQuality, 23);
  assert.equal(profile.encoder.peakBitrateMultiplier, 4);
  const { schemaVersion, ...effective } = profile;
  const runId = "123e4567-e89b-42d3-a456-426614174000";
  const status = { protocol: 2, runId, profileRevision: 1, phase: "applied", effective };
  assert.equal(benchStatusConfirmsApply(null, status, profile, runId), true);
  const stale = structuredClone(status);
  delete stale.effective.encoder.rateControlMode;
  delete stale.effective.encoder.targetQuality;
  delete stale.effective.encoder.peakBitrateMultiplier;
  assert.equal(benchStatusConfirmsApply(null, stale, profile, runId), false);
  assert.throws(() => benchmarkProfile({ ...candidate, targetQuality: 22 }, "wss"), /CQ23|bounded/i);
  assert.throws(() => benchmarkProfile({ ...candidate, roiMode: "feedback" }, "wss"), /experiment|axis|controls/i);
});

test("a shaped pipeline candidate retains its six-megabit cohort while lowering only the encoder target", () => {
  const candidate = {
    schemaVersion: 2, implementationVariant: "candidate", experimentId: "hybrid-cache-calibrated",
    preset: "p3", tuning: "ultra-low-latency", multipass: "disabled", aqMode: "spatial",
    aqStrength: 8, vbvFrames: 1, lookaheadFrames: 0, weightedPrediction: false,
    bitrateMbps: 5.4, requestedBitrateMbps: 6, totalMediaCapMbps: 6, gopSeconds: 2,
    wssTopology: { laneCount: 16, dataShards: 15, parityShards: 1 },
    hybridMode: "png-quiescent-detail", hybridCacheMode: "dda-gated",
  };

  const profile = benchmarkProfile(candidate, "wss");
  assert.equal(profile.bitrateBps, 5_400_000);
  assert.equal(profile.requestedBitrateBps, 6_000_000);
  assert.equal(profile.totalMediaCapBps, 6_000_000);
  assert.throws(() => benchmarkProfile({ ...candidate, requestedBitrateMbps: 5.4 }, "wss"),
    /experiment|control|6 Mbps/i);
  assert.throws(() => benchmarkProfile({ ...candidate, totalMediaCapMbps: 4 }, "wss"),
    /experiment|control|6 Mbps/i);
  assert.throws(() => benchmarkProfile({ ...candidate, bitrateMbps: 6.1 }, "wss"),
    /experiment|control|6 Mbps/i);
});

test("cap changes shape through the helper before exactly one fresh WSS transport epoch", async () => {
  const events = [];
  const session = new LiveStreamLabSession({
    runId: "123e4567-e89b-42d3-a456-426614174000",
    store: { appendTelemetry() {} },
    helperSession: { async shape(_runId, cap) { events.push(`shape:${cap}`); return { ok: true, event: "shaped" }; } },
    qosPolicyProbe: cap => ({ active: cap !== null, bandwidthMbps: cap }),
  });
  session.restartTransport = async () => { events.push("restart"); };
  session.transport = "wss";
  await session.setBandwidthCap(4);
  await session.setBandwidthCap(4);
  await session.setBandwidthCap(null);
  assert.deepEqual(events, ["shape:4", "restart", "shape:null", "restart"]);
});

test("a missing helper shape acknowledgement fails closed before transport restart", async () => {
  let restarted = false;
  const session = new LiveStreamLabSession({
    runId: "123e4567-e89b-42d3-a456-426614174000",
    store: { appendTelemetry() {} },
    helperSession: { async shape() { return null; } },
  });
  session.restartTransport = async () => { restarted = true; };
  await assert.rejects(() => session.setBandwidthCap(2), error => error?.code === "bandwidth-shaping-unverified");
  assert.equal(restarted, false);
});

test("performance summary uses host window duration for offered bytes and browser deltas for delivered bytes", () => {
  const windowMetrics = { windowDurationMs: 1_000, encodedBytes: 100_000, wssPayloadBytes: 140_000,
    encodeCount: 30, encodeUsTotal: 90_000, encodeUsMax: 5_000, idrCount: 1, deltaCount: 29,
    qpMapFrames: 12, qpMapChangedCtbs: 480, qpMapTotalCtbs: 12_240,
    encodeStages: {
      frames: 30,
      register: { count: 1, totalUs: 120, maxUs: 120 },
      map: { count: 30, totalUs: 600, maxUs: 30 },
      submit: { count: 30, totalUs: 60_000, maxUs: 2_400 },
      lock: { count: 30, totalUs: 24_000, maxUs: 1_100 },
      copy: { count: 30, totalUs: 300, maxUs: 20 },
      unmap: { count: 30, totalUs: 450, maxUs: 25 },
      unregister: { count: 1, totalUs: 80, maxUs: 80 },
    },
    roiActivity: {
      mode: "damage", evaluatedFrames: 14, mappedFrames: 12, missingDamageFrames: 1,
      emptyDamageFrames: 0, idrFrames: 1, coverageRejectedFrames: 0, unsupportedFrames: 0,
      source: "gpu-temporal", temporalFrames: 14, temporalPrimingFrames: 1,
      analysisP95Us: 1_200, analysisMaxUs: 1_400,
      lastSequence: 44, lastReason: "applied", lastChangedCtbs: 40, lastTotalCtbs: 2_040,
      lastMinDelta: -3, lastMaxDelta: 1,
    },
    hybridActivity: {
      mode: "png-quiescent-detail", frames: 14, historyResets: 0, historyPreservedFrames: 14,
      motionBypasses: 0, cropRemaps: 0, cursorNeutralFrames: 12, candidates: 2,
      reissuedTiles: 0, upserts: 2, invalidations: 0, budgetSkippedUpserts: 0,
      cache: { s: 10, r: 8, i: 1, x: 1, b: 5_000, a: 1_000 },
      pngBytes: 12_000, pngUsTotal: 2_000, pngUsMax: 1_200,
      analysisP95Us: 1_100, analysisMaxUs: 1_400,
      validationFrames: 14, validationUnavailableFrames: 0, validationFailedFrames: 0,
      deferredValidationReadyFrames: 12, deferredValidationPendingFrames: 2,
      deferredValidationAgeP95Frames: 2, deferredValidationAgeMaxFrames: 3,
      deferredValidationPendingMaxAgeFrames: 4,
      damageAuditFrames: 12, damageAuditMetadataUnavailableFrames: 1,
      damageAuditChangedCtbs: 120, damageAuditCoveredCtbs: 116, damageAuditMissedCtbs: 4,
      validationSubmitP95Us: 1_080, validationSubmitMaxUs: 1_350,
      validationCollectP95Us: 20, validationCollectMaxUs: 50,
      lastValidationSequence: 44, sidecarBytes: 12_400,
    } };
  const samples = [
    { ...sample(0, 100, 1, windowMetrics), wssMedia: { receivedBytes: 10_000, receivedPackets: 1 } },
    { ...sample(250, 108, 1, windowMetrics), wssMedia: { receivedBytes: 20_000, receivedPackets: 2 } },
    { ...sample(1_000, 130, 2, windowMetrics), wssMedia: { receivedBytes: 110_000, receivedPackets: 10 } },
    { ...sample(2_000, 160, 3, windowMetrics), wssMedia: { receivedBytes: 210_000, receivedPackets: 20 } },
    { ...sample(3_000, 190, 4, windowMetrics), wssMedia: { receivedBytes: 310_000, receivedPackets: 30 } },
    { ...sample(4_000, 220, 5, windowMetrics), wssMedia: { receivedBytes: 410_000, receivedPackets: 40 } },
    { ...sample(5_000, 250, 6, windowMetrics), wssMedia: { receivedBytes: 510_000, receivedPackets: 50 } },
  ];
  const metrics = summarizePerformance(samples, { transport: "wss", firstFrameMs: 900,
    bandwidthMbps: 1, qosPolicy: { active: true, bandwidthMbps: 1 } });
  assert.equal(metrics.presentedFps, 30);
  assert.equal(metrics.encodedBitrateMbps, 0.8);
  assert.equal(metrics.offeredWireBitrateMbps, 1.12);
  assert.equal(metrics.deliveredWireBitrateMbps, 0.8);
  assert.equal(metrics.wireBitrateMbps, 0.8);
  assert.equal(metrics.wireExpansion, 1.4);
  assert.equal(metrics.bandwidthVerified, true);
  assert.equal(metrics.encodeP95Ms, 5);
  assert.equal(metrics.qpMapFrames, 72);
  assert.equal(metrics.qpMapChangedCtbs, 2_880);
  assert.equal(metrics.qpMapTotalCtbs, 73_440);
  assert.equal(metrics.roiActivity.evaluatedFrames, 84);
  assert.equal(metrics.roiActivity.mappedFrames, 72);
  assert.equal(metrics.roiActivity.source, "gpu-temporal");
  assert.equal(metrics.roiActivity.temporalFrames, 84);
  assert.equal(metrics.roiActivity.temporalPrimingFrames, 6);
  assert.equal(metrics.roiActivity.analysisP95Us, 1_200);
  assert.equal(metrics.roiActivity.analysisMaxUs, 1_400);
  assert.equal(metrics.roiActivity.lastSequence, 44);
  assert.equal(metrics.roiActivity.lastReason, "applied");
  assert.equal(metrics.hostHybridActivity.validationFrames, 84);
  assert.equal(metrics.hostHybridActivity.validationUnavailableFrames, 0);
  assert.equal(metrics.hostHybridActivity.validationCollectP95Us, 20);
  assert.equal(metrics.hostHybridActivity.deferredValidationReadyFrames, 72);
  assert.equal(metrics.hostHybridActivity.deferredValidationPendingFrames, 12);
  assert.equal(metrics.hostHybridActivity.deferredValidationAgeP95Frames, 2);
  assert.equal(metrics.hostHybridActivity.deferredValidationAgeMaxFrames, 3);
  assert.equal(metrics.hostHybridActivity.deferredValidationPendingMaxAgeFrames, 4);
  assert.equal(metrics.hostHybridActivity.damageAuditFrames, 72);
  assert.equal(metrics.hostHybridActivity.damageAuditMetadataUnavailableFrames, 6);
  assert.equal(metrics.hostHybridActivity.damageAuditChangedCtbs, 720);
  assert.equal(metrics.hostHybridActivity.damageAuditCoveredCtbs, 696);
  assert.equal(metrics.hostHybridActivity.damageAuditMissedCtbs, 24);
  assert.equal(metrics.hostHybridActivity.lastValidationSequence, 44);
  assert.equal(metrics.hostHybridActivity.suppressions, 60);
  assert.equal(metrics.hostHybridActivity.reactivations, 48);
  assert.equal(metrics.hostHybridActivity.permanentInvalidations, 6);
  assert.equal(metrics.hostHybridActivity.staleReactivationRejects, 6);
  assert.equal(metrics.hostHybridActivity.cachedBytesRetained, 5_000);
  assert.equal(metrics.hostHybridActivity.pngBytesAvoided, 6_000);
  assert.equal(metrics.encodeStages.frames, 180);
  assert.equal(metrics.encodeStages.register.count, 6);
  assert.equal(metrics.encodeStages.register.meanMs, 0.12);
  assert.equal(metrics.encodeStages.register.maxMs, 0.12);
  assert.equal(metrics.encodeStages.map.count, 180);
  assert.equal(metrics.encodeStages.map.meanMs, 0.02);
  assert.equal(metrics.encodeStages.map.maxMs, 0.03);
  assert.equal(metrics.encodeStages.unregister.count, 6);
  assert.equal(metrics.firstFrameMs, 900);
  assert.equal(metrics.latencyP95Ms, 120);
  assert.equal(metrics.frameGapMaxMs, 80);
  assert.equal(metrics.encodedBitrateCv, 0);
  assert.equal(metrics.hostCpuPct, 50);
  assert.ok(Math.abs(metrics.gpuUtilizationMeanPct - 38.5714) < 0.001);
  assert.equal(metrics.privateMemoryPeakMb, 120);
});

test("performance summary merges backward-compatible temporal ROI metrics into activity", () => {
  const status = {
    windowDurationMs: 1_000, encodedBytes: 100_000, wssPayloadBytes: 140_000,
    encodeCount: 30, encodeUsTotal: 90_000, encodeUsMax: 5_000, idrCount: 1, deltaCount: 29,
    qpMapFrames: 28, qpMapChangedCtbs: 355, qpMapTotalCtbs: 57_120,
    roiActivity: {
      mode: "damage", evaluatedFrames: 30, mappedFrames: 28, missingDamageFrames: 0,
      emptyDamageFrames: 0, idrFrames: 1, coverageRejectedFrames: 1, unsupportedFrames: 0,
      lastSequence: 30, lastReason: "applied", lastChangedCtbs: 9, lastTotalCtbs: 2_040,
      lastMinDelta: -3, lastMaxDelta: 1,
    },
    metrics: {
      temporalRoi: {
        source: "gpu-temporal", temporalFrames: 30, temporalPrimingFrames: 1,
        analysisP95Us: 730, analysisMaxUs: 905,
      },
    },
  };
  const samples = [
    sample(0, 100, 6, status),
    sample(1_000, 130, 6, status),
  ];
  const metrics = summarizePerformance(samples, { transport: "wss", firstFrameMs: 900 });
  assert.equal(metrics.qpMapFrames, 28);
  assert.equal(metrics.qpMapChangedCtbs, 355);
  assert.equal(metrics.qpMapTotalCtbs, 57_120);
  assert.equal(metrics.encodedBitrateMbps, 0.8);
  assert.equal(metrics.offeredWireBitrateMbps, 1.12);
  assert.deepEqual(metrics.roiActivity, {
    ...status.roiActivity,
    ...status.metrics.temporalRoi,
  });
});

test("performance summary aggregates reconstruction-feedback counters and bounded timings", () => {
  const status = {
    windowDurationMs: 1_000, encodedBytes: 100_000, wssPayloadBytes: 140_000,
    encodeCount: 30, encodeUsTotal: 90_000, encodeUsMax: 5_000, idrCount: 1, deltaCount: 29,
    qpMapFrames: 20, qpMapChangedCtbs: 200, qpMapTotalCtbs: 40_800,
    roiActivity: {
      mode: "feedback", evaluatedFrames: 30, mappedFrames: 20, missingDamageFrames: 9,
      emptyDamageFrames: 0, idrFrames: 1, coverageRejectedFrames: 0, unsupportedFrames: 0,
      source: "gpu-temporal", temporalFrames: 30, temporalPrimingFrames: 1,
      analysisP95Us: 700, analysisMaxUs: 900, reconstructionActive: true,
      feedbackSubmittedFrames: 29, feedbackReadyFrames: 27, feedbackLateFrames: 1,
      feedbackSkippedFrames: 0, feedbackAdjustedFrames: 20, feedbackWarmupFrames: 8,
      feedbackMotionBypasses: 0, feedbackMissingFrames: 2, feedbackEligibleCtbs: 120,
      feedbackRefinedCtbs: 20, feedbackFundedCtbs: 40, feedbackSubmitP95Us: 80,
      feedbackSubmitMaxUs: 110, feedbackReadbackP95Us: 60, feedbackReadbackMaxUs: 90,
      lastFeedbackSequence: 29, lastFeedbackAgeFrames: 1,
    },
  };
  const metrics = summarizePerformance([
    sample(0, 100, 1, status),
    sample(1_000, 130, 2, status),
    sample(2_000, 160, 3, status),
  ], { transport: "wss", firstFrameMs: 900 });

  assert.equal(metrics.roiActivity.feedbackSubmittedFrames, 87);
  assert.equal(metrics.roiActivity.feedbackReadyFrames, 81);
  assert.equal(metrics.roiActivity.feedbackAdjustedFrames, 60);
  assert.equal(metrics.roiActivity.feedbackFundedCtbs, 120);
  assert.equal(metrics.roiActivity.feedbackSubmitP95Us, 80);
  assert.equal(metrics.roiActivity.feedbackReadbackMaxUs, 90);
  assert.equal(metrics.roiActivity.reconstructionActive, true);
  assert.equal(metrics.roiActivity.lastFeedbackSequence, 29);
  assert.equal(metrics.roiActivity.lastFeedbackAgeFrames, 1);
});

test("performance summary preserves exact WSS immediate-presentation counter deltas", () => {
  const status = {
    windowDurationMs: 1_000, encodedBytes: 100_000, wssPayloadBytes: 110_000,
    encodeCount: 30, encodeUsTotal: 90_000, encodeUsMax: 5_000, idrCount: 1, deltaCount: 29,
  };
  const samples = [
    {
      ...sample(0, 100, 1, status),
      framesReceived: 100,
      wssMedia: { receivedBytes: 10_000, presentation: {
        protocol: 1, immediateAttempts: 100, immediatePresented: 100,
        rafEnqueued: 0, rafCoalesced: 0, rafLateDrops: 0,
      } },
    },
    {
      ...sample(1_000, 130, 2, status),
      framesReceived: 130,
      wssMedia: { receivedBytes: 120_000, presentation: {
        protocol: 1, immediateAttempts: 130, immediatePresented: 130,
        rafEnqueued: 0, rafCoalesced: 0, rafLateDrops: 0,
      } },
    },
  ];

  const metrics = summarizePerformance(samples, { transport: "wss" });
  assert.deepEqual(metrics.wssPresentation, {
    protocol: 1,
    decodedFrames: 30,
    presentedFrames: 30,
    immediateAttempts: 30,
    immediatePresented: 30,
    rafEnqueued: 0,
    rafCoalesced: 0,
    rafLateDrops: 0,
  });
});

test("performance summary reports continuous WSS stage and per-lane arrival distributions", () => {
  const windowMetrics = { windowDurationMs: 1_000, encodedBytes: 100_000, wssPayloadBytes: 110_000,
    encodeCount: 30, encodeUsTotal: 90_000, encodeUsMax: 5_000, idrCount: 1, deltaCount: 29 };
  const topology = { laneCount: 6, dataShards: 5, parityShards: 1 };
  const withTrace = (elapsedMs, frames, sequence, firstShardAtMs, reconstructedAtMs, decodedAtMs,
    presentedAtMs, firstQueueDelayMs, completingQueueDelayMs, laneTelemetry) => ({
    ...sample(elapsedMs, frames, sequence, windowMetrics),
    wssMedia: {
      receivedBytes: elapsedMs * 100 + 10_000,
      receivedPackets: sequence,
      protocol: 3,
      topology,
      ...laneTelemetry,
    },
    frameTrace: {
      identity: null,
      trace: {
        phase: "performance-unmatched",
        encode: { encodedBytes: 5_000 },
        network: {
          sequence,
          sourceToFirstShardMs: firstShardAtMs / 10,
          sourceToHostEgressMs: firstShardAtMs / 25,
          hostEgressToEdgeMs: firstShardAtMs / 100,
          hostEgressToFirstShardMs: firstShardAtMs / 20,
          sourceToEdgeMs: firstShardAtMs / 20,
          edgeToFirstShardMs: firstShardAtMs / 25,
          firstShardAtMs,
          firstShardEventAtMs: firstShardAtMs - firstQueueDelayMs,
          firstShardHandlerAtMs: firstShardAtMs,
          firstShardQueueDelayMs: firstQueueDelayMs,
          reconstructingShardAtMs: reconstructedAtMs,
          completingShardEventAtMs: reconstructedAtMs - completingQueueDelayMs,
          completingShardHandlerAtMs: reconstructedAtMs,
          completingShardQueueDelayMs: completingQueueDelayMs,
          reconstructedAtMs,
          recovered: false,
        },
        decode: { queuedAtMs: reconstructedAtMs, decodedAtMs },
        presentation: { presentedAtMs, sourceToPresentedMs: presentedAtMs / 2 },
      },
    },
  });
  const samples = [
    withTrace(0, 0, 1, 100, 110, 114, 116, 1, 2, {
      reconstructed: 100,
      recoveredFrames: 80,
      highestSequenceByLane: [100, 100, 100, 100, 100, 100],
      completionMissingByLane: [50, 10, 5, 5, 0, 10],
      lateShardCountByLane: [50, 10, 5, 5, 0, 10],
      lateShardDelayMsTotalByLane: [100, 20, 5, 10, 0, 40],
    }),
    withTrace(1_000, 30, 2, 200, 220, 225, 228, 2, 4, {
      reconstructed: 130,
      recoveredFrames: 105,
      highestSequenceByLane: [130, 126, 130, 129, 130, 130],
      completionMissingByLane: [65, 12, 8, 8, 2, 15],
      lateShardCountByLane: [65, 12, 8, 8, 2, 15],
      lateShardDelayMsTotalByLane: [145, 24, 14, 16, 4, 50],
    }),
    withTrace(2_000, 60, 3, 300, 330, 336, 340, 3, 6, {
      reconstructed: 160,
      recoveredFrames: 130,
      highestSequenceByLane: [160, 152, 160, 158, 160, 160],
      completionMissingByLane: [80, 15, 10, 10, 5, 20],
      lateShardCountByLane: [80, 15, 10, 10, 5, 20],
      lateShardDelayMsTotalByLane: [190, 30, 20, 20, 10, 60],
    }),
  ];

  const metrics = summarizePerformance(samples, { transport: "wss", bandwidthMbps: 4 });
  assert.equal(metrics.performanceTraceSamples, 3);
  assert.equal(metrics.sourceToFirstShardP50Ms, 20);
  assert.equal(metrics.sourceToFirstShardP95Ms, 30);
  assert.equal(metrics.sourceToHostEgressP50Ms, 8);
  assert.equal(metrics.sourceToHostEgressP95Ms, 12);
  assert.equal(metrics.hostEgressToEdgeP50Ms, 2);
  assert.equal(metrics.hostEgressToEdgeP95Ms, 3);
  assert.equal(metrics.hostEgressToFirstShardP50Ms, 10);
  assert.equal(metrics.hostEgressToFirstShardP95Ms, 15);
  assert.equal(metrics.sourceToEdgeP50Ms, 10);
  assert.equal(metrics.sourceToEdgeP95Ms, 15);
  assert.equal(metrics.edgeToFirstShardP50Ms, 8);
  assert.equal(metrics.edgeToFirstShardP95Ms, 12);
  assert.equal(metrics.sourceToPresentedP50Ms, 114);
  assert.equal(metrics.sourceToPresentedP95Ms, 170);
  assert.equal(metrics.networkAssemblyP50Ms, 20);
  assert.equal(metrics.networkAssemblyP95Ms, 30);
  assert.equal(metrics.networkSerializationEstimateP50Ms, 8);
  assert.equal(metrics.networkSerializationEstimateP95Ms, 8);
  assert.equal(metrics.networkAssemblyResidualP50Ms, 12);
  assert.equal(metrics.networkAssemblyResidualP95Ms, 22);
  assert.equal(metrics.browserEventQueueDelayP50Ms, 2);
  assert.equal(metrics.browserEventQueueDelayP95Ms, 6);
  assert.equal(metrics.browserFirstShardQueueP50Ms, 2);
  assert.equal(metrics.browserFirstShardQueueP95Ms, 3);
  assert.equal(metrics.browserCompletingShardQueueP50Ms, 4);
  assert.equal(metrics.browserCompletingShardQueueP95Ms, 6);
  assert.equal(metrics.decodeP50Ms, 5);
  assert.equal(metrics.decodeP95Ms, 6);
  assert.equal(metrics.presentationP50Ms, 3);
  assert.equal(metrics.presentationP95Ms, 4);
  assert.equal(metrics.shardToPresentP50Ms, 28);
  assert.equal(metrics.shardToPresentP95Ms, 40);
  assert.deepEqual(metrics.wssLaneTelemetry, {
    topology,
    reconstructedFrames: 60,
    parityRecoveredFrames: 50,
    parityRecoveryRatio: 5 / 6,
    sequenceLagSampleCount: 3,
    sequenceLagMeanByLane: [0, 4, 0, 1, 0, 0],
    sequenceLagP95ByLane: [0, 8, 0, 2, 0, 0],
    sequenceLagMaxByLane: [0, 8, 0, 2, 0, 0],
    sequenceLagPositiveShareByLane: [0, 2 / 3, 0, 2 / 3, 0, 0],
    completionMissingByLane: [30, 5, 5, 5, 5, 10],
    completionShareByLane: [0.5, 1 / 12, 1 / 12, 1 / 12, 1 / 12, 1 / 6],
    lateShardCountByLane: [30, 5, 5, 5, 5, 10],
    lateShardDelayMsTotalByLane: [90, 10, 15, 10, 10, 20],
    lateShardDelayMsMeanByLane: [3, 2, 3, 2, 2, 2],
  });
});

test("wire budgeting can go below the experiment floor and converges toward ninety percent of the cap", () => {
  const v2 = { laneCount: 4, dataShards: 3, parityShards: 1 };
  assert.equal(initialEncoderTargetMbps(6, 2, v2), 1.1);
  assert.equal(nextStabilizedBitrateMbps(1.1, 2.2, 2), 0.9);
  assert.equal(nextStabilizedBitrateMbps(1, 0.5, 2), 1.8);
  assert.equal(nextStabilizedBitrateMbps(0.5, 4, 2), 0.5);
});

test("a matching six-megabit total cap starts from the unchanged encoder target", () => {
  const control = { laneCount: 16, dataShards: 15, parityShards: 1 };
  assert.equal(initialEncoderTargetMbps(6, 6, control), 6);
  assert.equal(initialEncoderTargetMbps(6, 4, control), 2.8);
});

test("a verified six-megabit candidate does not drift below its control target", async () => {
  const applied = [];
  const session = {
    receiverOptions: { wssTopology: { laneCount: 16, dataShards: 15, parityShards: 1 } },
    qosPolicy: { active: true, bandwidthMbps: 6 },
    lastFirstFrameMs: 100,
    async apply(configuration) { applied.push(configuration.bitrateMbps); },
    async collectPerformance() {
      const samples = [];
      for (let second = 0; second <= 5; second++) samples.push({
        ...sample(second * 1_000, second * 30, second + 1, {
          windowDurationMs: 1_000,
          encodedBytes: 620_000,
          wssPayloadBytes: 700_000,
          encodeCount: 30,
          encodeUsTotal: 60_000,
          encodeUsMax: 3_000,
          idrCount: 1,
          deltaCount: 29,
        }),
        queued: 0,
        wssMedia: { receivedBytes: second * 687_500, receivedPackets: second * 100 },
      });
      return { samples, actionToPhotonSamples: [] };
    },
  };
  const configuration = {
    schemaVersion: 2,
    implementationVariant: "candidate",
    experimentId: "hybrid-png-quiescent-detail",
    preset: "p3",
    tuning: "ultra-low-latency",
    multipass: "disabled",
    aqMode: "spatial",
    aqStrength: 8,
    vbvFrames: 1,
    lookaheadFrames: 0,
    weightedPrediction: false,
    bitrateMbps: 6,
    gopSeconds: 2,
    hybridMode: "png-quiescent-detail",
    wssTopology: { laneCount: 16, dataShards: 15, parityShards: 1 },
  };

  const result = await stabilizeEncoderForCap(session, configuration, 6, { stabilizationSeconds: 5 });

  assert.equal(result.stabilizedEncoderBitrateMbps, 6);
  assert.deepEqual(applied, [6]);
});

test("a pipeline candidate survives a cap-driven encoder correction without leaving its cohort", async () => {
  const applied = [];
  let target = 6;
  const session = {
    receiverOptions: { wssTopology: { laneCount: 16, dataShards: 15, parityShards: 1 } },
    qosPolicy: { active: true, bandwidthMbps: 6 },
    lastFirstFrameMs: 100,
    async apply(configuration) {
      benchmarkProfile(configuration, "wss");
      target = configuration.bitrateMbps;
      applied.push(configuration);
    },
    async collectPerformance() {
      const offered = target === 6 ? 6.2 : 5.4;
      const delivered = Math.min(5.8, offered);
      const samples = [];
      for (let second = 0; second <= 5; second++) samples.push({
        ...sample(second * 1_000, second * 30, second + 1, {
          windowDurationMs: 1_000,
          encodedBytes: Math.round((offered / 1.1) * 1_000_000 / 8),
          wssPayloadBytes: Math.round(offered * 1_000_000 / 8),
          encodeCount: 30, encodeUsTotal: 60_000, encodeUsMax: 3_000, idrCount: 1, deltaCount: 29,
        }),
        queued: 0,
        wssMedia: {
          receivedBytes: Math.round(second * delivered * 1_000_000 / 8),
          receivedPackets: second * 100,
        },
      });
      return { samples, actionToPhotonSamples: [] };
    },
  };
  const configuration = {
    schemaVersion: 2, implementationVariant: "candidate", experimentId: "hybrid-cache-calibrated",
    preset: "p3", tuning: "ultra-low-latency", multipass: "disabled", aqMode: "spatial",
    aqStrength: 8, vbvFrames: 1, lookaheadFrames: 0, weightedPrediction: false,
    bitrateMbps: 6, gopSeconds: 2,
    hybridMode: "png-quiescent-detail", hybridCacheMode: "suppress-reactivate",
    wssTopology: { laneCount: 16, dataShards: 15, parityShards: 1 },
  };

  const result = await stabilizeEncoderForCap(session, configuration, 6, { stabilizationSeconds: 5 });

  assert.equal(applied.length, 2);
  assert.equal(applied[0].bitrateMbps, 6);
  assert.ok(applied[1].bitrateMbps < 6);
  assert.ok(applied.every(value => value.requestedBitrateMbps === 6 && value.totalMediaCapMbps === 6));
  assert.equal(result.requestedBitrateMbps, 6);
  assert.ok(result.stabilizedEncoderBitrateMbps < 6);
});

test("shaping validation fails closed on missing policy, delivered overshoot, offered overshoot, or backlog", () => {
  const healthy = { deliveredWireBitrateMbps: 3.9, offeredWireBitrateMbps: 3.7,
    queueSlopePerMinute: 0, latencyP95Ms: 120, dependencyLosses: 0, decoderResets: 0, freezes: 0 };
  assert.deepEqual(shapingFailureReasons(healthy, 4, { active: true, bandwidthMbps: 4 }), []);
  assert.ok(shapingFailureReasons(healthy, 4, null).includes("qos-policy-unverified"));
  assert.ok(shapingFailureReasons({ ...healthy, deliveredWireBitrateMbps: 4.3 }, 4,
    { active: true, bandwidthMbps: 4 }).includes("delivered-cap-overshoot"));
  assert.ok(shapingFailureReasons({ ...healthy, offeredWireBitrateMbps: 4 }, 4,
    { active: true, bandwidthMbps: 4 }).includes("offered-cap-overshoot"));
  assert.ok(shapingFailureReasons({ ...healthy, queueSlopePerMinute: 0.1 }, 4,
    { active: true, bandwidthMbps: 4 }).includes("queue-growth"));
});

test("verified shaping reports decoder resets as a runtime hard-gate failure", async () => {
  const qosPolicy = { active: true, bandwidthMbps: 6 };
  const windowMetrics = {
    windowDurationMs: 1_000, encodedBytes: 100_000, wssPayloadBytes: 140_000,
    encodeCount: 30, encodeUsTotal: 60_000, encodeUsMax: 3_000,
    idrCount: 1, deltaCount: 29,
  };
  let performancePass = 0;
  let qualityStarted = false;
  const session = {
    receiverOptions: { wssTopology: { laneCount: 16, dataShards: 15, parityShards: 1 } },
    qosPolicy,
    lastFirstFrameMs: 100,
    async apply() {},
    async collectPerformance() {
      performancePass += 1;
      const samples = [];
      for (let second = 0; second <= 5; second++) samples.push({
        ...sample(second * 1_000, second * 30, second + 1, windowMetrics),
        resyncs: performancePass === 2 && second === 5 ? 2 : 0,
        queued: 0,
        wssMedia: { receivedBytes: second * 100_000, receivedPackets: second * 100 },
      });
      return { samples, actionToPhotonSamples: [] };
    },
    async collectQuality() { qualityStarted = true; return { samples: [] }; },
  };
  const configuration = {
    schemaVersion: 2, implementationVariant: "candidate", experimentId: "runtime-gate",
    preset: "p3", tuning: "ultra-low-latency", multipass: "disabled", aqMode: "spatial",
    aqStrength: 8, vbvFrames: 1, lookaheadFrames: 0, weightedPrediction: false,
    bitrateMbps: 6, gopSeconds: 2,
  };

  await assert.rejects(
    () => liveModule.measureOptimizationCandidate(session, configuration, 6, {
      stabilizationSeconds: 5, optimizationPerformanceSeconds: 5,
    }),
    error => error?.code === "performance-hard-gate-failed" &&
      error?.detail?.metrics?.bandwidthVerified === true &&
      error?.detail?.reasons?.includes("decoder-reset"),
  );
  assert.equal(performancePass, 2);
  assert.equal(qualityStarted, false);
});

test("shaped encoder stabilization makes at most two corrections and never captures quality on failure", async () => {
  const applied = [];
  const session = {
    receiverOptions: { wssTopology: { laneCount: 4, dataShards: 3, parityShards: 1 } },
    qosPolicy: { active: true, bandwidthMbps: 2 },
    lastFirstFrameMs: 500,
    async apply(configuration) { applied.push(configuration.bitrateMbps); },
    async collectPerformance() {
      const samples = [];
      for (let second = 0; second <= 5; second++) samples.push({
        ...sample(second * 1_000, second * 30, second + 1, {
          windowDurationMs: 1_000, encodedBytes: 150_000, wssPayloadBytes: 275_000,
          encodeCount: 30, encodeUsTotal: 60_000, encodeUsMax: 3_000,
          idrCount: second === 0 ? 1 : 0, deltaCount: 30,
        }),
        queued: 0,
        wssMedia: { receivedBytes: second * 225_000, receivedPackets: second * 100 },
      });
      return { samples, actionToPhotonSamples: [] };
    },
  };
  await assert.rejects(() => stabilizeEncoderForCap(session, {
    schemaVersion: 2, implementationVariant: "candidate", experimentId: "stabilize-fail",
    preset: "p3", tuning: "ultra-low-latency", multipass: "disabled", aqMode: "spatial",
    aqStrength: 8, vbvFrames: 1, lookaheadFrames: 0, weightedPrediction: false,
    bitrateMbps: 6, gopSeconds: 2,
  }, 2, { stabilizationSeconds: 5 }), error => error?.code === "bandwidth-shaping-unverified");
  assert.equal(applied.length, 3);
  assert.equal(applied[0], 1.1);
  assert.ok(applied.every(value => value >= 0.5));
});

test("a repeated cap starts from the session's previously proven safe encoder target", async () => {
  const applied = [];
  let target = null;
  const session = {
    receiverOptions: { wssTopology: { laneCount: 4, dataShards: 3, parityShards: 1 } },
    qosPolicy: { active: true, bandwidthMbps: 2 },
    lastFirstFrameMs: 100,
    async apply(configuration) { target = configuration.bitrateMbps; applied.push(target); },
    async collectPerformance() {
      const offered = target === 1.1 ? 1.92 : target === 1 ? 1.91 : 1.55;
      const delivered = Math.min(1.9, offered);
      const samples = [];
      for (let second = 0; second <= 5; second++) samples.push({
        ...sample(second * 1_000, second * 30, second + 1, {
          windowDurationMs: 1_000,
          encodedBytes: Math.round((offered / 1.35) * 1_000_000 / 8),
          wssPayloadBytes: Math.round(offered * 1_000_000 / 8),
          encodeCount: 30, encodeUsTotal: 60_000, encodeUsMax: 3_000, idrCount: 1, deltaCount: 29,
        }),
        queued: 0,
        wssMedia: { receivedBytes: Math.round(second * delivered * 1_000_000 / 8), receivedPackets: second * 100 },
      });
      return { samples, actionToPhotonSamples: [] };
    },
  };
  const configuration = {
    schemaVersion: 2, implementationVariant: "production", experimentId: "reuse-safe-target",
    preset: "p3", tuning: "ultra-low-latency", multipass: "disabled", aqMode: "spatial",
    aqStrength: 8, vbvFrames: 1, lookaheadFrames: 0, weightedPrediction: false,
    bitrateMbps: 6, gopSeconds: 2,
  };
  const first = await stabilizeEncoderForCap(session, configuration, 2, { stabilizationSeconds: 5 });
  const second = await stabilizeEncoderForCap(session, configuration, 2, { stabilizationSeconds: 5 });
  assert.equal(first.stabilizedEncoderBitrateMbps, 0.9);
  assert.equal(second.initialEncoderBitrateMbps, 0.9);
  assert.deepEqual(applied, [1.1, 1, 0.9, 0.9]);
});

test("a repeated cap reuses an upward-stabilized encoder target without drifting back down", async () => {
  const applied = [];
  let target = null;
  const session = {
    receiverOptions: { wssTopology: { laneCount: 16, dataShards: 15, parityShards: 1 } },
    qosPolicy: { active: true, bandwidthMbps: 4 },
    lastFirstFrameMs: 100,
    async apply(configuration) { target = configuration.bitrateMbps; applied.push(target); },
    async collectPerformance() {
      const offered = target === 2.8 ? 3.34 : 3.55;
      const samples = [];
      for (let second = 0; second <= 5; second++) samples.push({
        ...sample(second * 1_000, second * 30, second + 1, {
          windowDurationMs: 1_000,
          encodedBytes: Math.round((offered / 1.1) * 1_000_000 / 8),
          wssPayloadBytes: Math.round(offered * 1_000_000 / 8),
          encodeCount: 30, encodeUsTotal: 60_000, encodeUsMax: 3_000, idrCount: 1, deltaCount: 29,
        }),
        queued: 0,
        wssMedia: { receivedBytes: Math.round(second * offered * 1_000_000 / 8), receivedPackets: second * 100 },
      });
      return { samples, actionToPhotonSamples: [] };
    },
  };
  const configuration = {
    schemaVersion: 2, implementationVariant: "production", experimentId: "reuse-upward-target",
    preset: "p3", tuning: "ultra-low-latency", multipass: "disabled", aqMode: "spatial",
    aqStrength: 8, vbvFrames: 1, lookaheadFrames: 0, weightedPrediction: false,
    bitrateMbps: 6, gopSeconds: 2,
    wssTopology: { laneCount: 16, dataShards: 15, parityShards: 1 },
  };
  const first = await stabilizeEncoderForCap(session, configuration, 4, { stabilizationSeconds: 5 });
  const second = await stabilizeEncoderForCap(session, configuration, 4, { stabilizationSeconds: 5 });
  assert.equal(first.stabilizedEncoderBitrateMbps, 3);
  assert.equal(second.initialEncoderBitrateMbps, 3);
  assert.deepEqual(applied, [2.8, 3, 3]);
});

test("post-apply readiness requires a newer full effective benchmark profile", () => {
  const expected = benchmarkProfile({ preset: "p4", tuning: "low-latency", multipass: "quarter",
    aqStrength: 10, temporalAq: true, vbvFrames: 2, bitrateMbps: 4, gopSeconds: 2 }, "wss");
  const status = { protocol: 1, runId: "123e4567-e89b-42d3-a456-426614174000", profileRevision: 8, sequence: 1,
    phase: "sample", effective: { ...expected, encoder: expected.encoder } };
  assert.equal(benchStatusConfirmsApply({ ...status, profileRevision: 7, sequence: 99 }, status, expected, status.runId), true);
  assert.equal(benchStatusConfirmsApply(status, status, expected, status.runId), false);
  assert.equal(benchStatusConfirmsApply({ ...status, profileRevision: 7 }, { ...status, effective: { ...status.effective, bitrateBps: 3_000_000 } }, expected, status.runId), false);
  const { gopSeconds, ...missingGop } = status.effective;
  assert.equal(benchStatusConfirmsApply({ ...status, profileRevision: 7 }, { ...status, effective: missingGop }, expected, status.runId), false);
  assert.equal(benchStatusConfirmsApply(null, { ...status, profileRevision: undefined }, expected, status.runId), false);
  assert.equal(benchStatusConfirmsApply(null, { ...status, profileRevision: 8.5 }, expected, status.runId), false);
  assert.equal(benchStatusConfirmsApply({ ...status, profileRevision: undefined, sequence: 1 },
    { ...status, profileRevision: undefined, sequence: 2 }, expected, status.runId), false);
});

test("post-apply readiness treats reordered WSS topology fields as the same profile", () => {
  const expected = benchmarkProfile({ schemaVersion: 2, implementationVariant: "candidate",
    experimentId: "dirty-qp-map", preset: "p3", tuning: "ultra-low-latency",
    multipass: "disabled", aqMode: "spatial", aqStrength: 8, vbvFrames: 1,
    lookaheadFrames: 0, weightedPrediction: false, dirtyQpMap: true, main10: false,
    bitrateMbps: 6, gopSeconds: 2,
    wssTopology: { laneCount: 16, dataShards: 15, parityShards: 1 } }, "wss");
  const runId = "ee1d494a-541f-430e-a9a6-2d82e6125cc7";
  const { schemaVersion, adaptive, wssTopology, ...effective } = expected;
  const status = {
    protocol: 2,
    runId,
    profileRevision: 1,
    sequence: 3,
    phase: "sample",
    effective: {
      ...effective,
      wssTopology: {
        dataShards: wssTopology.dataShards,
        laneCount: wssTopology.laneCount,
        parityShards: wssTopology.parityShards,
      },
    },
  };

  assert.equal(benchStatusConfirmsApply(null, status, expected, runId), true);
});

test("feedback profile admission waits for an active reconstruction analyzer", () => {
  const expected = benchmarkProfile({ schemaVersion: 2, implementationVariant: "candidate",
    experimentId: "feedback-roi", preset: "p3", tuning: "ultra-low-latency",
    multipass: "disabled", aqMode: "spatial", aqStrength: 8, vbvFrames: 1,
    lookaheadFrames: 0, weightedPrediction: false, roiMode: "feedback", main10: false,
    bitrateMbps: 6, gopSeconds: 2,
    wssTopology: { laneCount: 16, dataShards: 15, parityShards: 1 } }, "wss");
  const runId = "ee1d494a-541f-430e-a9a6-2d82e6125cc7";
  const { schemaVersion, adaptive, ...effective } = expected;
  const status = { protocol: 2, runId, profileRevision: 1, sequence: 3, phase: "sample",
    effective, roiActivity: { mode: "feedback", reconstructionActive: false } };

  assert.equal(benchStatusConfirmsApply(null, status, expected, runId), false);
  assert.equal(benchStatusConfirmsApply(null, { ...status,
    roiActivity: { ...status.roiActivity, reconstructionActive: true } }, expected, runId), true);
});

test("candidate apply confirms profile and media priming before triggering the measured source frame", async () => {
  const events = [];
  const session = Object.create(LiveStreamLabSession.prototype);
  session.transport = "wss";
  session.applyCandidateProfile = async () => {
    events.push("profile-confirmed");
    return { preApplyBenchStatus: { sequence: 4 } };
  };
  session.waitForMediaPrimed = async (_candidate, _transport, readiness) => {
    assert.equal(readiness.preApplyBenchStatus.sequence, 4);
    events.push("media-primed");
  };
  session.source = { evaluate: async () => {
    events.push("source-triggered");
    return { epochMs: 1234 };
  } };
  session.waitForReady = async () => { events.push("first-frame-ready"); };
  await session.apply({ id: "candidate" });
  assert.deepEqual(events, [
    "profile-confirmed", "media-primed", "source-triggered", "first-frame-ready",
  ]);
});

test("media priming reselects the persistent VDD after signaling reconnect", async () => {
  const candidate = {
    preset: "p3", tuning: "ultra-low-latency", multipass: "disabled",
    aqStrength: 8, vbvFrames: 1, bitrateMbps: 6,
  };
  const expected = benchmarkProfile(candidate, "wss");
  const runId = "123e4567-e89b-42d3-a456-426614174001";
  const benchStatus = {
    protocol: 2,
    runId,
    profileRevision: 1,
    sequence: 9,
    phase: "sample",
    effective: { ...expected, encoder: expected.encoder },
  };
  const unselected = {
    route: "wss",
    dec: "hevc wss",
    renderedTimestamp: null,
    benchStatus,
  };
  const selected = {
    ...unselected,
    renderedTimestamp: 100,
    presented: 1,
  };
  const session = Object.create(LiveStreamLabSession.prototype);
  session.runId = runId;
  session.assertLeaseHealthy = () => {};
  let snapshotCalls = 0;
  session.snapshot = async () => {
    snapshotCalls++;
    if (snapshotCalls > 1) throw new Error("monitor selection was not checked before retrying priming");
    return unselected;
  };
  let ensureCalls = 0;
  session.ensureVirtualMonitorSelected = async snapshot => {
    assert.equal(snapshot, unselected);
    ensureCalls++;
    return { snapshot: selected, reselected: true };
  };

  const primed = await session.waitForMediaPrimed(candidate, "wss");

  assert.equal(primed, selected);
  assert.equal(ensureCalls, 1);
});

test("first-frame readiness reselects the persistent VDD after signaling reconnect", async () => {
  const candidate = {
    preset: "p3", tuning: "ultra-low-latency", multipass: "disabled",
    aqStrength: 8, vbvFrames: 1, bitrateMbps: 6,
  };
  const expected = benchmarkProfile(candidate, "wss");
  const runId = "123e4567-e89b-42d3-a456-426614174000";
  const benchStatus = {
    protocol: 1,
    runId,
    profileRevision: 1,
    sequence: 9,
    phase: "sample",
    effective: { ...expected, encoder: expected.encoder },
  };
  const session = Object.create(LiveStreamLabSession.prototype);
  session.runId = runId;
  session.assertLeaseHealthy = () => {};
  session.store = { appendTelemetry() {} };
  session.applyExpectedSource = { epochMs: 100 };
  session.firstFrameStartedAt = performance.now();
  let snapshotCount = 0;
  session.snapshot = async () => ({
    route: "wss",
    dec: "hev1.1.6.L120.B0",
    renderedTimestamp: 100,
    glassToGlass: { timestamp: snapshotCount++ === 0 ? 100 : 200 },
    benchStatus,
  });
  let ensureCalls = 0;
  session.ensureVirtualMonitorSelected = async snapshot => ({
    snapshot,
    reselected: ensureCalls++ === 0,
  });
  let sourceTriggers = 0;
  session.source = {
    async evaluate(script) {
      assert.match(script, /triggerAction/);
      sourceTriggers++;
      return { epochMs: 200 };
    },
  };

  await session.waitForReady(candidate, "wss");

  assert.equal(ensureCalls, 2);
  assert.equal(sourceTriggers, 1);
  assert.equal(session.applyExpectedSource.epochMs, 200);
});

test("measured first-frame retry is bounded and only repairs an otherwise primed stale stream", () => {
  const expected = benchmarkProfile({ preset: "p3", tuning: "ultra-low-latency", multipass: "disabled",
    aqStrength: 8, vbvFrames: 1, bitrateMbps: 2.2, gopSeconds: 2 }, "wss");
  const runId = "123e4567-e89b-42d3-a456-426614174000";
  const benchStatus = { protocol: 2, runId, profileRevision: 2, sequence: 9, phase: "sample",
    effective: { ...expected, encoder: expected.encoder } };
  const state = firstFrameReadiness({ route: "wss", dec: "hev1.1.6.L120.B0", renderedTimestamp: 100,
    glassToGlass: { timestamp: 99 }, benchStatus }, {
    expected, preApplyBenchStatus: { ...benchStatus, profileRevision: 1, sequence: 99 }, runId,
    transport: "wss", expectedTimestamp: 101,
  });
  assert.deepEqual(state, { rendered: true, routeOkay: true, decoderOkay: true, benchOkay: true, fresh: false, ready: false });
  assert.equal(shouldRetryMeasuredFrame(state, 999, 0), false);
  assert.equal(shouldRetryMeasuredFrame(state, 1_000, 0), true);
  assert.equal(shouldRetryMeasuredFrame(state, 5_000, 1), false);
  assert.equal(shouldRetryMeasuredFrame({ ...state, routeOkay: false }, 1_000, 0), false);
});

test("native readiness proves presented HEVC frames instead of the HUD label", () => {
  const expected = benchmarkProfile({ preset: "p3", tuning: "ultra-low-latency", multipass: "disabled",
    aqStrength: 8, vbvFrames: 1, bitrateMbps: 2.2, gopSeconds: 2 }, "webrtc");
  const runId = "123e4567-e89b-42d3-a456-426614174000";
  const benchStatus = { protocol: 2, runId, profileRevision: 2, sequence: 9, phase: "sample",
    effective: { ...expected, encoder: expected.encoder } };
  const snapshot = { route: "host->srflx", dec: "native video", renderedTimestamp: 120, renderedAgeMs: 120,
    nativePresentation: { generation: 4, supported: true, framesPresented: 12, width: 1920, height: 1080,
      firstFrameAtMs: 1_700_000_000_000, firstFramePerformanceMs: 5_000, lastFrameAtMs: 5_120 },
    rtc: { generation: 4, codec: { mimeType: "video/H265" },
      inbound: { timestamp: 1_700_000_000_500, framesDecoded: 12 } },
    glassToGlass: { timestamp: 101 }, benchStatus };
  const options = { expected, preApplyBenchStatus: { ...benchStatus, profileRevision: 1, sequence: 99 },
    runId, transport: "webrtc", relay: false, decoderPath: "native", expectedTimestamp: 101 };
  const state = firstFrameReadiness(snapshot, options);
  assert.equal(state.decoderOkay, true);
  assert.equal(state.ready, true);
  assert.deepEqual(nativeFrameState(snapshot), {
    supported: true, presented: true, sized: true, firstFrameRecorded: true, codecOkay: true,
    epochMatches: true, statsAfterFirstFrame: true, decoded: true, freshFrame: true, ready: true });
  // Chromium reports inbound.timestamp as wall-clock epoch ms, so the ordering
  // check uses firstFrameAtMs and ignores the rVFC time-origin clock.
  assert.equal(nativeFrameState({ ...snapshot,
    nativePresentation: { ...snapshot.nativePresentation, firstFramePerformanceMs: null } }).ready, true);
  const unproven = firstFrameReadiness({ ...snapshot, nativePresentation: null, renderedTimestamp: null }, options);
  assert.equal(unproven.decoderOkay, false);
  assert.equal(unproven.ready, false);
  // Missing nullable timestamps are not coerced to zero and must fail closed.
  const missingFirstFrame = firstFrameReadiness({ ...snapshot,
    nativePresentation: { ...snapshot.nativePresentation, firstFrameAtMs: null } }, options);
  assert.equal(missingFirstFrame.decoderOkay, false);
  assert.equal(nativeFrameState({ ...snapshot,
    nativePresentation: { ...snapshot.nativePresentation, firstFrameAtMs: null } }).firstFrameRecorded, false);
  assert.equal(nativeFrameState({ ...snapshot, renderedAgeMs: null }).freshFrame, false);
  assert.equal(nativeFrameState({ ...snapshot,
    nativePresentation: { ...snapshot.nativePresentation, width: null, height: null } }).sized, false);
  // Presented frames on a non-HEVC codec are not HEVC acceptance.
  const h264 = firstFrameReadiness({ ...snapshot, rtc: { ...snapshot.rtc, codec: { mimeType: "video/H264" } } }, options);
  assert.equal(h264.decoderOkay, false);
  // RTC stats left over from the previous peer are not evidence either.
  const previousPeer = firstFrameReadiness({ ...snapshot,
    rtc: { ...snapshot.rtc, generation: 3, inbound: { timestamp: 1_700_000_001_000, framesDecoded: 900 } } }, options);
  assert.equal(previousPeer.decoderOkay, false);
  const staleSample = firstFrameReadiness({ ...snapshot,
    rtc: { ...snapshot.rtc, inbound: { timestamp: 1_699_999_999_000, framesDecoded: 12 } } }, options);
  assert.equal(staleSample.decoderOkay, false);
  const undecoded = firstFrameReadiness({ ...snapshot,
    rtc: { ...snapshot.rtc, inbound: { timestamp: 1_700_000_000_500, framesDecoded: 0 } } }, options);
  assert.equal(undecoded.decoderOkay, false);
  // A frozen frame from this epoch is still not a live stream.
  const frozen = firstFrameReadiness({ ...snapshot, renderedAgeMs: NATIVE_RENDERED_AGE_LIMIT_MS + 1 }, options);
  assert.equal(frozen.decoderOkay, false);
  // The WebCodecs diagnostic path keeps its HUD-label contract.
  const diagnostic = firstFrameReadiness({ ...snapshot, dec: "hev1.1.6.L120.B0" },
    { ...options, decoderPath: "webcodecs" });
  assert.equal(diagnostic.decoderOkay, true);
  // Switching back to WSS retains the RTC decoder preference, but WSS still
  // decodes through WebCodecs and must not require native video callbacks.
  const wss = firstFrameReadiness({ ...snapshot, route: "wss", dec: "hev1.1.6.L120.B0",
    nativePresentation: null, rtc: null }, { ...options, transport: "wss" });
  assert.equal(wss.decoderOkay, true);
});

test("native-to-WSS navigation retains the lab v3 contract before host capabilities arrive", async () => {
  const urls = [];
  const session = {
    transport: "wss", receiverOptions: normalizeReceiverOptions(),
    runId: "123e4567-e89b-42d3-a456-426614174000", baseUrl: "https://play.example/",
    assertLeaseHealthy() {}, async waitForAuthentication() {},
    receiver: { async navigate(url) { urls.push(new URL(url)); } },
  };
  await liveModule.LiveStreamLabSession.prototype.switchTransport.call(session, "webrtc",
    { relay: false, decoderPath: "native" });
  await liveModule.LiveStreamLabSession.prototype.switchTransport.call(session, "wss", { adaptive: false });
  assert.equal(urls[0].searchParams.get("decoderPath"), "native");
  assert.equal(urls[0].searchParams.has("wssLanes"), false);
  assert.equal(urls[1].searchParams.get("wssLanes"), "16");
  assert.equal(urls[1].searchParams.get("wssParity"), "1");
  assert.equal(urls[1].searchParams.get("decoderPath"), "webcodecs");
  assert.equal(session.receiverOptions.decoderPath, "native");
});

test("native playback cannot enter the source-barcode benchmark", async () => {
  await assert.rejects(() => liveModule.LiveStreamLabSession.prototype.applyCandidateProfile.call(
    { receiverOptions: { decoderPath: "native" }, transport: "webrtc" }, { codec: "hevc" }),
    error => error.code === "native-benchmark-unavailable");
});

test("startup classification distinguishes a missing recovery ACK from a silent DDA session", () => {
  assert.equal(typeof liveModule.classifySilentDdaFailure, "function");
  const state = {
    rendered: true,
    routeOkay: true,
    decoderOkay: true,
    benchOkay: true,
    fresh: false,
    ready: false,
  };
  const diagnostics = {
    renderedAgeMs: 1_600,
    benchStatus: { encodeCount: 0, encodedBytes: 0, wssPayloadBytes: 0 },
    wssMedia: {
      openLanes: 16,
      receivedPackets: 32,
      reconstructed: 2,
      presented: 2,
      highestSequenceByLane: Array(16).fill(2),
      acknowledgements: {
        attempts: 1,
        sent: 1,
        failed: 0,
        last: { generation: 2, reconstructed: 2, sent: true },
      },
      dependencyLosses: 0,
      packetRejects: { total: 0 },
      topology: { laneCount: 16, dataShards: 15, parityShards: 1 },
    },
  };
  const expectedSource = { frameId: 613, epochMs: 1_000 };
  const movingSource = { frameId: 690, epochMs: 2_000, paused: false };

  assert.deepEqual(liveModule.classifySilentDdaFailure(
    state, diagnostics, expectedSource, movingSource, 1_500), {
    code: "dda-session-stalled",
    sourceFrameDelta: 77,
    renderedAgeMs: 1_600,
    openLanes: 16,
    receivedPackets: 32,
  });
  const legacyRecoveryAckBlocked = {
    ...diagnostics,
    wssMedia: {
      ...diagnostics.wssMedia,
      receivedPackets: 16,
      reconstructed: 1,
      presented: 1,
      highestSequenceByLane: Array(16).fill(1),
      acknowledgements: null,
    },
  };
  assert.deepEqual(liveModule.classifySilentDdaFailure(
    state, legacyRecoveryAckBlocked, expectedSource, movingSource, 1_500), {
    code: "wss-recovery-ack-stalled",
    sourceFrameDelta: 77,
    renderedAgeMs: 1_600,
    openLanes: 16,
    receivedPackets: 16,
    reconstructed: 1,
    presented: 1,
    ackDisposition: "uninstrumented",
    acknowledgements: null,
  });
  const recoveryAckNotAttempted = {
    ...legacyRecoveryAckBlocked,
    wssMedia: {
      ...legacyRecoveryAckBlocked.wssMedia,
      acknowledgements: { attempts: 0, sent: 0, failed: 0, last: null },
    },
  };
  assert.deepEqual(liveModule.classifySilentDdaFailure(
    state, recoveryAckNotAttempted, expectedSource, movingSource, 1_500), {
    code: "wss-recovery-ack-stalled",
    sourceFrameDelta: 77,
    renderedAgeMs: 1_600,
    openLanes: 16,
    receivedPackets: 16,
    reconstructed: 1,
    presented: 1,
    ackDisposition: "not-attempted",
    acknowledgements: { attempts: 0, sent: 0, failed: 0, last: null },
  });
  assert.equal(liveModule.classifySilentDdaFailure(
    state, diagnostics, expectedSource, movingSource, 1_249), null);
  assert.equal(liveModule.classifySilentDdaFailure(
    state, diagnostics, expectedSource, { ...movingSource, frameId: 613 }, 1_500), null);
  assert.equal(liveModule.classifySilentDdaFailure(
    state, { ...diagnostics, benchStatus: { ...diagnostics.benchStatus, encodeCount: 1 } },
    expectedSource, movingSource, 1_500), null);
});

test("a confirmed zero-output candidate startup is rejected without masking infrastructure failures", () => {
  const configuration = { implementationVariant: "candidate", experimentId: "enc-work-001" };
  const error = Object.assign(new Error("prime failed"), { code: "media-prime-timeout", detail: { diagnostics: {
    route: "wss", signaling: { registered: true }, wssMedia: { receivedBytes: 0, receivedPackets: 0 },
    benchStatus: { implementationVariant: "candidate", experimentId: "enc-work-001",
      encodedBytes: 0, wssPayloadBytes: 0, encodeCount: 0 },
  } } });
  assert.deepEqual(candidateStartupRejection(error, configuration, null), {
    result: "rejected", experimentId: "enc-work-001", implementationVariant: "candidate",
    configuration, bandwidthMbps: null,
    rejection: { code: "encoder-candidate-startup-failed", sourceCode: "media-prime-timeout",
      message: "prime failed", encodedBytes: 0, offeredBytes: 0, encodeCount: 0 },
  });
  assert.equal(candidateStartupRejection({ ...error, code: "bandwidth-shaping-unverified" }, configuration, 4), null);
  assert.equal(candidateStartupRejection(error, { ...configuration, implementationVariant: "production" }, null), null);
  assert.equal(candidateStartupRejection(error, { ...configuration, experimentId: "different" }, null), null);
  const produced = Object.assign(new Error("prime failed"), { code: "media-prime-timeout", detail: { diagnostics: {
    ...error.detail.diagnostics,
    benchStatus: { ...error.detail.diagnostics.benchStatus, encodedBytes: 1 },
  } } });
  assert.equal(candidateStartupRejection(produced, configuration, null), null);
  const afterPrime = Object.assign(new Error("fresh frame failed"), { code: "first-frame-timeout",
    detail: { ...error.detail.diagnostics, renderedAgeMs: 15_000,
      wssMedia: { receivedBytes: 1_255_382, receivedPackets: 323 } } });
  assert.equal(candidateStartupRejection(afterPrime, configuration, null)?.rejection.code,
    "encoder-candidate-startup-failed");
  const neverRendered = Object.assign(new Error("no frame"), { code: "first-frame-timeout",
    detail: { ...error.detail.diagnostics, renderedAgeMs: null } });
  assert.equal(candidateStartupRejection(neverRendered, configuration, null)?.rejection.code,
    "encoder-candidate-startup-failed");
  const advancing = Object.assign(new Error("fresh frame pending"), { code: "first-frame-timeout",
    detail: { ...afterPrime.detail, renderedAgeMs: 100 } });
  assert.equal(candidateStartupRejection(advancing, configuration, null), null);
});

test("screening checkpoints an incompatible encoder candidate and propagates non-candidate failures", async () => {
  const configuration = { implementationVariant: "candidate", experimentId: "enc-work-001", bitrateMbps: 2 };
  const startupFailure = Object.assign(new Error("NVENC startup failed"), { code: "media-prime-timeout", detail: { diagnostics: {
    route: "wss", signaling: { registered: true }, wssMedia: { receivedBytes: 0, receivedPackets: 0 },
    benchStatus: { implementationVariant: "candidate", experimentId: "enc-work-001",
      encodedBytes: 0, wssPayloadBytes: 0, encodeCount: 0 },
  } } });
  const telemetry = [];
  const rejected = await measureOptimizationScreenCandidate({
    apply: async () => { throw startupFailure; }, store: { appendTelemetry: event => telemetry.push(event) },
  }, configuration, null, {}, "screen-001");
  assert.equal(rejected.result, "rejected");
  assert.equal(rejected.rejection.code, "encoder-candidate-startup-failed");
  assert.equal(telemetry[0].phase, "encoder-candidate-rejected");

  const mediaGateFailure = Object.assign(new Error("candidate reset the decoder"), {
    code: "performance-hard-gate-failed",
    detail: {
      bandwidthMbps: 4,
      qosPolicy: { active: true, bandwidthMbps: 4 },
      metrics: { bandwidthVerified: true, decoderResets: 2 },
      reasons: ["decoder-reset"],
    },
  });
  const runtimeRejected = await measureOptimizationScreenCandidate({
    apply: async () => { throw mediaGateFailure; }, store: { appendTelemetry: event => telemetry.push(event) },
  }, configuration, 4, {}, "screen-001");
  assert.equal(runtimeRejected.result, "rejected");
  assert.equal(runtimeRejected.rejection.code, "encoder-candidate-runtime-failed");
  assert.equal(runtimeRejected.rejection.sourceCode, "performance-hard-gate-failed");
  assert.deepEqual(runtimeRejected.rejection.reasons, ["decoder-reset"]);

  const ownedFrameTimeout = Object.assign(new Error("decoded frame stalled"), {
    code: "frame-match-timeout",
    detail: { diagnostics: {
      route: "wss", dec: "hev1.1.6.L120.B0", renderedAgeMs: 5_064,
      signaling: { registered: true }, wssMedia: { receivedBytes: 13_288_770, receivedPackets: 1_132 },
      benchStatus: { implementationVariant: "candidate", experimentId: "enc-work-001" },
    } },
  });
  const frameTimeoutRejected = await measureOptimizationScreenCandidate({
    apply: async () => { throw ownedFrameTimeout; }, store: { appendTelemetry: event => telemetry.push(event) },
  }, configuration, null, {}, "screen-001");
  assert.equal(frameTimeoutRejected.result, "rejected");
  assert.equal(frameTimeoutRejected.rejection.code, "encoder-candidate-runtime-failed");
  assert.deepEqual(frameTimeoutRejected.rejection.reasons, ["frame-match-timeout"]);

  const unownedFrameTimeout = Object.assign(new Error("unknown frame stalled"), {
    code: "frame-match-timeout", detail: { diagnostics: {
      route: "wss", signaling: { registered: true }, wssMedia: { receivedBytes: 100 },
      benchStatus: { implementationVariant: "candidate", experimentId: "different-candidate" },
    } },
  });
  await assert.rejects(() => measureOptimizationScreenCandidate({
    apply: async () => { throw unownedFrameTimeout; }, store: { appendTelemetry() {} },
  }, configuration, null, {}, "screen-001"), error => error === unownedFrameTimeout);

  const infrastructureFailure = Object.assign(new Error("QoS missing"), { code: "bandwidth-shaping-unverified" });
  await assert.rejects(() => measureOptimizationScreenCandidate({
    apply: async () => { throw infrastructureFailure; }, store: { appendTelemetry() {} },
  }, configuration, null, {}, "screen-001"), error => error === infrastructureFailure);
});

test("the optimizer recovers a poisoned candidate host through a known-good profile before continuing", () => {
  const source = readFileSync(new URL("../tools/stream-lab-live.mjs", import.meta.url), "utf8");
  const screening = source.slice(source.indexOf("const screenRuns = []"), source.indexOf("allRuns.push(...screenRuns)"));
  const rejected = screening.indexOf("shouldRecoverRejectedCandidate(run, wasCompleted)");
  const recover = screening.indexOf("await session.recoverAfterRejectedCandidate(baselineConfiguration)");
  const compare = screening.indexOf("evaluateOptimizationRun");
  assert.ok(rejected >= 0 && recover > rejected && compare > recover);
});

test("resume does not replay host recovery for cached rejected candidates", () => {
  const rejected = { result: "rejected" };
  assert.equal(shouldRecoverRejectedCandidate(rejected, false), true);
  assert.equal(shouldRecoverRejectedCandidate(rejected, true), false);
  assert.equal(shouldRecoverRejectedCandidate({ result: "complete" }, false), false);
});

test("cached checkpoints reconcile live state but do not repeat measurements", async () => {
  const events = [];
  const cached = { unit: { result: "complete", value: 7 } };
  const result = await runCheckpointedUnit({ completedUnits: cached, onCheckpoint: async () => {} }, "unit",
    async () => events.push("reconciled"), async () => events.push("measured"));
  assert.deepEqual(result, cached.unit);
  assert.deepEqual(events, ["reconciled"]);
});

test("fresh checkpoints retain measurements while tagging activity contamination", async () => {
  const snapshots = [
    { sequence: 4, lastPhysicalAt: 1_000, observedAt: 1_100, active: true },
    { sequence: 7, lastPhysicalAt: 1_900, observedAt: 2_000, active: true },
  ];
  const completedUnits = {};
  const result = await runCheckpointedUnit({ completedUnits,
    activitySnapshot: () => snapshots.shift(), onCheckpoint: async () => {} }, "unit", null,
    async () => ({ result: "complete", latencyP95Ms: 222, vmafP5: 91 }));
  assert.equal(result.latencyP95Ms, 222);
  assert.equal(result.vmafP5, 91);
  assert.equal(result.performanceContaminated, true);
  assert.equal(result.latencyAccuracy, "possibly-inaccurate");
  assert.deepEqual(result.activityEvidence, {
    policy: "tag", startSequence: 4, endSequence: 7,
    lastPhysicalAt: 1_900, performanceContaminated: true,
  });
  assert.deepEqual(completedUnits.unit, result);
});

test("termination cleanup is graceful once and a second signal hard-exits", async () => {
  const target = new EventEmitter();
  let release;
  const cleanup = new Promise(resolve => { release = resolve; });
  const exits = [];
  const dispose = installTerminationCleanup(() => cleanup, { target, hardExit: code => exits.push(code), timeoutMs: 10_000 });
  target.emit("SIGINT");
  await Promise.resolve();
  assert.deepEqual(exits, []);
  target.emit("SIGTERM");
  assert.deepEqual(exits, [143]);
  release();
  await Promise.resolve();
  dispose();
});

test("signal cleanup records an interrupted checkpoint instead of leaving a campaign running", () => {
  const next = interruptedCampaignState({ status: "running", units: { baseline: {} }, pauseState: {
    paused: false, reason: null, since: null, resumeAfter: null,
  } }, "SIGINT", "2026-08-26T14:00:00.000Z");
  assert.equal(next.status, "interrupted");
  assert.equal(next.error.code, "interrupted");
  assert.equal(next.interruptedAt, "2026-08-26T14:00:00.000Z");
  assert.deepEqual(next.units, { baseline: {} });
});

test("lease heartbeat serializes renewals, surfaces failure, and stop awaits in-flight work", async () => {
  const scheduled = [];
  let resolveRenewal;
  const heartbeat = new SerializedLeaseHeartbeat(() => new Promise(resolve => { resolveRenewal = resolve; }), {
    schedule: callback => { scheduled.push(callback); return callback; }, cancel: () => {}, intervalMs: 240_000,
  });
  heartbeat.start();
  assert.equal(scheduled.length, 1);
  scheduled.shift()();
  assert.equal(scheduled.length, 0, "no second renewal may be scheduled while one is in flight");
  let stopped = false;
  const stopping = heartbeat.stop().then(() => { stopped = true; });
  await Promise.resolve();
  assert.equal(stopped, false);
  resolveRenewal();
  await stopping;
  assert.equal(stopped, true);

  const failed = new SerializedLeaseHeartbeat(async () => { throw new Error("lease lost"); }, {
    schedule: callback => { queueMicrotask(callback); return callback; }, cancel: () => {}, intervalMs: 1,
  });
  failed.start();
  await new Promise(resolve => setImmediate(resolve));
  assert.throws(() => failed.assertHealthy(), /lease lost/);
  await assert.rejects(failed.stop(), /lease lost/);
});

test("session cleanup waits for an in-flight local renewal before deleting scratch", async () => {
  const root = mkdtempSync(join(tmpdir(), "bubbleplay-lab-stop-"));
  const scratch = join(root, "scratch");
  mkdirSync(scratch);
  let releaseRenewal;
  const events = [];
  const session = new LiveStreamLabSession({
    runId: "123e4567-e89b-42d3-a456-426614174000",
    store: { appendTelemetry() {} },
    env: { LOCALAPPDATA: root },
  });
  session.runScratch = scratch;
  session.heartbeat = { stop: async () => {
    events.push("renewal-wait");
    await new Promise(resolve => { releaseRenewal = resolve; });
    events.push("renewal-finished");
  } };
  const stopping = session.stop();
  await new Promise(resolve => setImmediate(resolve));
  assert.deepEqual(events, ["renewal-wait"]);
  assert.equal(existsSync(scratch), true);
  releaseRenewal();
  await stopping;
  assert.deepEqual(events, ["renewal-wait", "renewal-finished"]);
  assert.equal(existsSync(scratch), false);
});

test("session cleanup completes before rethrowing an in-flight renewal failure", async () => {
  const root = mkdtempSync(join(tmpdir(), "bubbleplay-lab-renew-fail-"));
  const scratch = join(root, "scratch");
  mkdirSync(scratch);
  const events = [];
  const session = new LiveStreamLabSession({
    runId: "123e4567-e89b-42d3-a456-426614174000", store: { appendTelemetry() {} }, env: { LOCALAPPDATA: root },
  });
  session.runScratch = scratch;
  session.heartbeat = { stop: async () => { events.push("renewal-failed"); throw new Error("lease lost"); } };
  await assert.rejects(session.stop(), /lease lost/);
  assert.deepEqual(events, ["renewal-failed"]);
  assert.equal(existsSync(scratch), false);
});

test("cleanup evidence checks the helper, transient VDD, QoS policy, and both Chrome children", () => {
  const evidence = inspectCleanupResources({
    preDisplays: [{ deviceName: "\\\\.\\DISPLAY1" }],
    transientDisplay: { deviceName: "\\\\.\\DISPLAY2" },
    currentDisplays: [{ deviceName: "\\\\.\\DISPLAY1" }],
    service: { installed: true, queryable: true, state: 1, stateName: "stopped" },
    qosPolicies: [],
    childProcesses: [
      { role: "receiver", pid: 101, alive: false },
      { role: "source", pid: 102, alive: false },
    ],
  });
  assert.equal(evidence.serviceStopped, true);
  assert.equal(evidence.transientVddAbsent, true);
  assert.equal(evidence.qosPoliciesAbsent, true);
  assert.equal(evidence.chromeChildrenExited, true);
  const leaked = inspectCleanupResources({
    preDisplays: [{ deviceName: "\\\\.\\DISPLAY1" }],
    transientDisplay: { deviceName: "\\\\.\\DISPLAY2" },
    currentDisplays: [{ deviceName: "\\\\.\\DISPLAY1" }, { deviceName: "\\\\.\\DISPLAY2" }],
    service: { installed: true, queryable: true, state: 4, stateName: "running" },
    qosPolicies: ["BubblePlay-StreamLab-abc-lane-0"],
    childProcesses: [{ role: "receiver", pid: 101, alive: true }],
  });
  assert.equal(leaked.serviceStopped, false);
  assert.equal(leaked.transientVddAbsent, false);
  assert.equal(leaked.qosPoliciesAbsent, false);
  assert.equal(leaked.chromeChildrenExited, false);
});

test("cleanup treats a broker-owned persistent VDD remaining attached as healthy", () => {
  const display = { deviceName: "\\\\.\\DISPLAY5" };
  const evidence = inspectCleanupResources({
    preDisplays: [{ deviceName: "\\\\.\\DISPLAY1" }, display],
    transientDisplay: display,
    persistentDisplay: true,
    currentDisplays: [{ deviceName: "\\\\.\\DISPLAY1" }, display],
    service: { installed: true, queryable: true, state: 1, stateName: "stopped" },
    qosPolicies: [],
    childProcesses: [
      { role: "receiver", pid: 101, alive: false },
      { role: "source", pid: 102, alive: false },
    ],
  });
  assert.equal(evidence.persistentVddPresent, true);
  assert.equal(evidence.displayResourcesClean, true);
});

test("resume re-runs idle measurement and retains no historical resource telemetry", async () => {
  const events = [];
  const context = { completedUnits: { idle: { result: "complete", seconds: 60, sampleCount: 60,
    gpuEncoderMeanPct: 99, hostMemoryDeltaMb: 999, cleanupEvidence: { serviceStopped: false } } },
  onCheckpoint: async (_id, result) => events.push(["checkpoint", result.gpuEncoderMeanPct]) };
  const result = await runFreshCheckpoint(context, "idle", async () => events.push(["cleanup"]), async () => {
    events.push(["measured"]);
    return { result: "complete", seconds: 60, sampleCount: 60, gpuEncoderMeanPct: 0, hostMemoryDeltaMb: 0 };
  });
  assert.equal(result.gpuEncoderMeanPct, 0);
  assert.equal(result.resumeHistory.gpuEncoderMeanPct, undefined);
  assert.equal(result.resumeHistory.hostMemoryDeltaMb, undefined);
  assert.deepEqual(events, [["cleanup"], ["measured"], ["checkpoint", 0]]);
});

function sample(elapsedMs, frames, sequence, benchMetrics) {
  return {
    elapsedMs,
    framesDrawn: frames,
    glassToGlass: { ageMs: elapsedMs ? 120 : 100, sequence: frames },
    benchStatus: { runId: "123e4567-e89b-42d3-a456-426614174000", sequence, ...benchMetrics },
    maxFrameGap: 40 + elapsedMs / 125,
    system: {
      cpuSeconds: 10 + elapsedMs / 2000,
      privateMemoryBytes: elapsedMs === 5000 ? 120 * 1024 * 1024 : 100 * 1024 * 1024,
      gpuUtilizationPct: 30 + (elapsedMs ? 10 : 0), encoderUtilizationPct: 20,
      vramUsedMb: 512, powerWatts: 45,
    },
  };
}
