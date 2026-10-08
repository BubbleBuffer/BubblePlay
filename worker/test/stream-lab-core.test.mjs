import test from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import {
  SCENES,
  artifactStore,
  buildFfmpegQualityCommand,
  characterErrorRate,
  compareCandidates,
  evaluateCampaignAcceptance,
  scoreLumaTiles,
  successiveHalving,
  temporalUnchangedRegionError,
} from "../tools/lab/stream-lab-core.mjs";

test("synthetic corpus exposes stable named scenes", () => {
  assert.deepEqual(SCENES.map((scene) => scene.id), [
    "ui-text", "thin-lines", "gradients", "scrolling", "motion-60", "localized-change",
  ]);
  assert.ok(SCENES.every((scene) => scene.seed === 469));
});

test("campaign acceptance fails closed on unsupported, media gates, H264, and idle cleanup", () => {
  const pass = campaignFixture();
  assert.deepEqual(evaluateCampaignAcceptance(pass), { accepted: true, reasons: [] });
  const unsupported = structuredClone(pass);
  unsupported.stability.find(unit => unit.unitId === "stability:sharp-viewport-2x").result = "unsupported";
  assert.equal(evaluateCampaignAcceptance(unsupported).accepted, false);
  const slowGame = structuredClone(pass);
  slowGame.stability.find(unit => unit.unitId === "stability:wss-gaming").presentedFps = 20;
  assert.ok(evaluateCampaignAcceptance(slowGame).reasons.includes("stability:wss-gaming:low-presented-fps"));
  const badH264 = structuredClone(pass);
  badH264.stability.find(unit => unit.unitId === "stability:h264-fallback-smoke").codec = "hevc";
  assert.ok(evaluateCampaignAcceptance(badH264).reasons.includes("stability:h264-fallback-smoke:wrong-codec"));
  const busyIdle = structuredClone(pass);
  busyIdle.stability.find(unit => unit.unitId === "stability:post-cleanup-idle").gpuEncoderMeanPct = 12;
  assert.ok(evaluateCampaignAcceptance(busyIdle).reasons.includes("stability:post-cleanup-idle:encoder-busy"));
  const leaked = structuredClone(pass);
  leaked.stability.find(unit => unit.unitId === "stability:post-cleanup-idle").cleanupEvidence.serviceStopped = false;
  assert.ok(evaluateCampaignAcceptance(leaked).reasons.includes("stability:post-cleanup-idle:helper-running"));
});

test("OCR character error rate normalizes line endings and measures edits", () => {
  assert.equal(characterErrorRate("BubblePlay\r\nGröße", "BubblePlay\nGröße"), 0);
  assert.equal(characterErrorRate("BubblePlay", "BubblePloy"), 1 / 10);
  assert.equal(characterErrorRate("", "unexpected"), 1);
});

test("regional scoring exposes localized damage hidden by the whole frame", () => {
  const width = 80;
  const height = 40;
  const reference = new Uint8Array(width * height).fill(128);
  const candidate = reference.slice();
  for (let y = 10; y < 20; y++) for (let x = 20; x < 30; x++) candidate[y * width + x] = 0;

  const score = scoreLumaTiles(reference, candidate, width, height, { columns: 8, rows: 4 });
  assert.equal(score.tiles.length, 32);
  assert.ok(score.mean > 0.9, `whole-frame score should remain deceptively high: ${score.mean}`);
  assert.ok(score.worst < 0.1, `damaged tile must fail loudly: ${score.worst}`);
  assert.equal(score.p1, score.worst);
});

test("temporal scoring catches corruption in regions that should stay unchanged", () => {
  const width = 80, height = 40;
  const referenceA = new Uint8Array(width * height).fill(120);
  const referenceB = referenceA.slice();
  const decodedA = referenceA.slice();
  const decodedB = referenceB.slice();
  for (let y = 0; y < 10; y++) for (let x = 0; x < 10; x++) decodedB[y * width + x] = 220;
  const score = temporalUnchangedRegionError(referenceA, referenceB, decodedA, decodedB, width, height);
  assert.ok(score.mean < 0.02, `global temporal error should be diluted: ${score.mean}`);
  assert.ok(score.worstTile > 0.35, `localized temporal corruption should be visible: ${score.worstTile}`);
  assert.equal(score.unchangedPixels, width * height);
});

test("FFmpeg quality orchestration includes desktop and regional metrics", () => {
  const command = buildFfmpegQualityCommand("source.mkv", "decoded.mkv", "metrics.json");
  assert.equal(command.file, "ffmpeg");
  assert.match(command.args.join(" "), /libvmaf/);
  assert.match(command.args.join(" "), /ssim/);
  assert.match(command.args.join(" "), /psnr/);
  assert.match(command.args.join(" "), /blurdetect/);
  assert.match(command.args.join(" "), /blockdetect/);
  assert.match(command.args.join(" "), /blend=all_mode=difference/,
    "blocking must be measured on the reference-aligned distortion residual");
  assert.match(command.args.join(" "), /metrics\.json/);
});

test("comparison rejects hard-gate failures before quality gains", () => {
  const baseline = fixture({ id: "baseline" });
  const prettyButFrozen = fixture({
    id: "frozen",
    firstFrameMs: 6_000,
    ocrCer: 0.01,
    worstTileSsim: 0.99,
    vmaf: 99,
  });
  const result = compareCandidates(baseline, prettyButFrozen, { noise: {} });
  assert.equal(result.accepted, false);
  assert.ok(result.reasons.includes("first-frame-timeout"));
});

test("comparison accepts a material text improvement within latency noise", () => {
  const baseline = fixture({ id: "baseline", ocrCer: 0.08, worstTileSsim: 0.80, vmaf: 80 });
  const candidate = fixture({ id: "candidate", ocrCer: 0.05, worstTileSsim: 0.81, vmaf: 81, latencyP95Ms: 311 });
  const result = compareCandidates(baseline, candidate, {
    noise: { ocrCer: 0.002, worstTileSsim: 0.002, vmaf: 0.4, latencyP95Ms: 8 },
  });
  assert.equal(result.accepted, true);
  assert.equal(result.primaryImprovement, "ocr");
});

test("quiescent comparison uses its analysis and mouse-neutral latency contract", () => {
  const baseline = fixture({
    id: "baseline",
    actionToPhotonP95Ms: null,
    actionProbe: { result: "skipped", reason: "input-automation-disabled" },
    latencyP95Ms: 135,
    vmaf: 90,
  });
  const candidate = fixture({
    id: "quiescent-detail",
    configuration: { hybridMode: "png-quiescent-detail" },
    actionToPhotonP95Ms: null,
    actionProbe: { result: "skipped", reason: "input-automation-disabled" },
    encodeP95Ms: 12.5,
    latencyP95Ms: 144,
    hostHybridActivity: { frames: 271, analysisP95Us: 50 },
    vmaf: 94.5,
  });

  const result = compareCandidates(baseline, candidate, { noise: { latencyP95Ms: 0 } });

  assert.equal(result.accepted, true, result.reasons.join(", "));
  assert.equal(result.primaryImprovement, "vmaf");
});

test("legacy quiescent artifacts retain explicit mouse-neutral evidence", () => {
  const baseline = fixture({ id: "baseline", actionToPhotonP95Ms: null, latencyP95Ms: 135, vmaf: 90 });
  const candidate = fixture({
    id: "quiescent-detail",
    configuration: { hybridMode: "png-quiescent-detail" },
    actionToPhotonP95Ms: null,
    encodeP95Ms: 12.5,
    latencyP95Ms: 144,
    hostHybridActivity: { frames: 271, analysisP95Us: 50 },
    repetitions: [{
      quiescentInteractions: { skipped: true, reason: "input-automation-disabled" },
    }],
    vmaf: 94.5,
  });

  const result = compareCandidates(baseline, candidate, { noise: { latencyP95Ms: 0 } });

  assert.equal(result.accepted, true, result.reasons.join(", "));
});

test("quiescent comparison fails closed on missing or slow analysis", () => {
  const baseline = fixture({ id: "baseline", actionToPhotonP95Ms: null, vmaf: 90 });
  const candidate = fixture({
    id: "quiescent-detail",
    configuration: { hybridMode: "png-quiescent-detail" },
    actionToPhotonP95Ms: null,
    actionProbe: { result: "skipped", reason: "input-automation-disabled" },
    hostHybridActivity: { frames: 271, analysisP95Us: 1_501 },
    vmaf: 94.5,
  });
  const slow = compareCandidates(baseline, candidate);
  assert.ok(slow.reasons.includes("quiescent-analysis-overhead"));

  const missing = structuredClone(candidate);
  delete missing.hostHybridActivity.analysisP95Us;
  const absent = compareCandidates(baseline, missing);
  assert.ok(absent.reasons.includes("quiescent-analysis-missing"));
});

test("matched quiescent comparison requires material cache-efficiency reductions", () => {
  const common = {
    actionToPhotonP95Ms: null,
    actionProbe: { result: "skipped", reason: "input-automation-disabled" },
    bandwidthVerified: true,
  };
  const baseline = fixture({
    ...common,
    id: "dda-gated-control",
    configuration: {
      hybridMode: "png-quiescent-detail",
      hybridCacheMode: "dda-gated",
    },
    latencyP95Ms: 135,
    vmaf: 90,
    hostHybridActivity: {
      mode: "png-quiescent-detail",
      frames: 300,
      analysisP95Us: 450,
      upserts: 300,
      invalidations: 300,
      pngBytes: 1_200_000,
      pngUsTotal: 300_000,
      sidecarBytes: 1_250_000,
    },
  });
  const candidate = fixture({
    ...common,
    id: "cache-suppression",
    configuration: {
      hybridMode: "png-quiescent-detail",
      hybridCacheMode: "suppress-reactivate",
    },
    latencyP95Ms: 144,
    vmaf: 90,
    hostHybridActivity: {
      mode: "png-quiescent-detail",
      frames: 300,
      analysisP95Us: 50,
      upserts: 20,
      invalidations: 10,
      pngBytes: 100_000,
      pngUsTotal: 20_000,
      sidecarBytes: 120_000,
      suppressions: 200,
      reactivations: 196,
      permanentInvalidations: 4,
      pngBytesAvoided: 1_000_000,
    },
  });

  const accepted = compareCandidates(baseline, candidate, { noise: { latencyP95Ms: 0 } });
  assert.equal(accepted.accepted, true, accepted.reasons.join(", "));
  assert.equal(accepted.cacheEfficiency.minimumReduction, 0.5);
  assert.ok(accepted.cacheEfficiency.reductions.pngBytes > 0.9);
  assert.ok(accepted.cacheEfficiency.reductions.churn > 0.9);
  assert.ok(accepted.cacheEfficiency.reductions.sidecarBytes > 0.9);
  assert.ok(accepted.cacheEfficiency.reductions.hostWork > 0.9);

  const ineffective = structuredClone(candidate);
  Object.assign(ineffective.hostHybridActivity, {
    upserts: 250,
    invalidations: 240,
    pngBytes: 900_000,
    pngUsTotal: 240_000,
    sidecarBytes: 950_000,
  });
  const rejected = compareCandidates(baseline, ineffective, { noise: { latencyP95Ms: 0 } });
  assert.equal(rejected.accepted, false);
  assert.ok(rejected.reasons.includes("quiescent-cache-png-bytes"));
  assert.ok(rejected.reasons.includes("quiescent-cache-churn"));
  assert.ok(rejected.reasons.includes("quiescent-cache-sidecar-bytes"));
  assert.ok(rejected.reasons.includes("quiescent-cache-host-work"));

  const inactive = structuredClone(candidate);
  Object.assign(inactive.hostHybridActivity, {
    suppressions: 0,
    reactivations: 0,
    permanentInvalidations: 0,
    pngBytesAvoided: 0,
  });
  const missingActivity = compareCandidates(baseline, inactive, { noise: { latencyP95Ms: 0 } });
  assert.ok(missingActivity.reasons.includes("quiescent-cache-suppression-missing"));
  assert.ok(missingActivity.reasons.includes("quiescent-cache-resolution-missing"));
  assert.ok(missingActivity.reasons.includes("quiescent-cache-avoidance-missing"));

  const unverified = structuredClone(candidate);
  unverified.bandwidthVerified = null;
  const missingCapEvidence = compareCandidates(
    baseline, unverified, { noise: { latencyP95Ms: 0 } });
  assert.ok(missingCapEvidence.reasons.includes("quiescent-cache-bandwidth-unverified"));
});

test("quiescent cache-efficiency comparison rejects an unmatched cache-policy cohort", () => {
  const baseline = fixture({
    id: "wrong-control",
    configuration: {
      hybridMode: "png-quiescent-detail",
      hybridCacheMode: "suppress-reactivate",
    },
    hostHybridActivity: { frames: 30 },
  });
  const candidate = fixture({
    id: "suppression",
    configuration: {
      hybridMode: "png-quiescent-detail",
      hybridCacheMode: "suppress-reactivate",
    },
    hostHybridActivity: { frames: 30 },
  });

  const result = compareCandidates(baseline, candidate);

  assert.equal(result.accepted, false);
  assert.ok(result.reasons.includes("quiescent-cache-policy-mismatch"));
  assert.deepEqual(result.cacheEfficiency.expectedPolicies, {
    baseline: "dda-gated",
    candidate: "suppress-reactivate",
  });
});

test("legacy encode and measured-action latency gates remain enforced", () => {
  const baseline = fixture({ id: "baseline", actionToPhotonP95Ms: 100, vmaf: 90 });
  const candidate = fixture({
    id: "ordinary-candidate",
    actionToPhotonP95Ms: 121,
    encodeP95Ms: 10.1,
    vmaf: 94.5,
  });

  const result = compareCandidates(baseline, candidate);

  assert.ok(result.reasons.includes("encode-latency"));
  assert.ok(result.reasons.includes("action-latency"));
});

test("successive halving keeps valid candidates in Work-mode rank order", () => {
  const candidates = [
    fixture({ id: "latency", ocrCer: 0.06, worstTileSsim: 0.85, vmaf: 90, latencyP95Ms: 250 }),
    fixture({ id: "text", ocrCer: 0.03, worstTileSsim: 0.84, vmaf: 84, latencyP95Ms: 300 }),
    fixture({ id: "region", ocrCer: 0.04, worstTileSsim: 0.92, vmaf: 87, latencyP95Ms: 290 }),
    fixture({ id: "invalid", freezes: 1, ocrCer: 0 }),
  ];
  const survivors = successiveHalving(candidates, 3);
  assert.deepEqual(survivors.map((candidate) => candidate.id), ["text", "region", "latency"]);
});

test("artifact store writes manifest, telemetry, metrics, comparison, and ACK failure diagnostics", () => {
  const root = mkdtempSync(join(tmpdir(), "bubbleplay-stream-lab-"));
  try {
    const store = artifactStore(root, "run-123");
    store.initialize({ command: "baseline" });
    store.updateManifest({ candidateHost: {
      path: "C:\\lab\\bubbleplay-host.exe",
      sha256: "abc123",
      expectedCompatibilityId: "compat-v11",
      compatibilityMarkerPresent: true,
    } });
    store.appendTelemetry({ elapsedMs: 10, fps: 30 });
    store.writeMetrics({ result: "pass" });
    store.writeComparison({ accepted: true });
    const acknowledgements = {
      attempts: 1,
      sent: 0,
      failed: 1,
      last: { generation: 2, reconstructed: 1, sent: false },
    };
    store.writeFailure({
      code: "wss-recovery-ack-stalled",
      detail: { diagnostics: { wssMedia: { acknowledgements } } },
    });
    const manifest = JSON.parse(readFileSync(join(store.path, "manifest.json"), "utf8"));
    assert.equal(manifest.runId, "run-123");
    assert.equal(manifest.candidateHost.sha256, "abc123");
    assert.equal(manifest.candidateHost.compatibilityMarkerPresent, true);
    assert.deepEqual(JSON.parse(readFileSync(join(store.path, "telemetry.ndjson"), "utf8")), { elapsedMs: 10, fps: 30 });
    assert.equal(JSON.parse(readFileSync(join(store.path, "metrics.json"), "utf8")).result, "pass");
    assert.equal(JSON.parse(readFileSync(join(store.path, "comparison.json"), "utf8")).accepted, true);
    assert.deepEqual(
      JSON.parse(readFileSync(join(store.path, "failure-diagnostics.json"), "utf8"))
        .detail.diagnostics.wssMedia.acknowledgements,
      acknowledgements,
    );
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("campaign journal checkpoints candidates atomically and can resume", () => {
  const root = mkdtempSync(join(tmpdir(), "bubbleplay-stream-lab-journal-"));
  try {
    const store = artifactStore(root, "run-123");
    store.initialize({ command: "campaign" });
    store.writeCampaignState({ schemaVersion: 1, status: "running", units: {} });
    store.writeCampaignState({ schemaVersion: 1, status: "running", units: { "candidate:p3": { id: "p3", result: "complete" } } });
    const resumed = store.readCampaignState();
    assert.equal(resumed.units["candidate:p3"].result, "complete");
    assert.deepEqual(store.listTemporaryFiles(), []);
  } finally { rmSync(root, { recursive: true, force: true }); }
});

function fixture(overrides = {}) {
  return {
    id: "candidate",
    transport: "wss",
    codec: "hevc",
    firstFrameMs: 1_000,
    presentedFps: 30,
    encodeP95Ms: 5,
    latencyP95Ms: 300,
    actionToPhotonP95Ms: 300,
    freezes: 0,
    decoderResets: 0,
    dependencyLosses: 0,
    spatialTransitions: 0,
    queueSlopePerMinute: 0,
    memorySlopeMbPerMinute: 0,
    ocrCer: 0.06,
    worstTileSsim: 0.8,
    vmaf: 80,
    wireBitrateMbps: 4,
    ...overrides,
  };
}

function campaignFixture() {
  const baseline = fixture({ id: "baseline", latencyP95Ms: 120, actionToPhotonP95Ms: 140 });
  const media = (unitId, overrides = {}) => fixture({ unitId, result: "complete", firstFrameMs: 500,
    presentedFps: unitId.includes("gaming") ? 60 : 30, encodeP95Ms: 4, latencyP95Ms: 130,
    actionToPhotonP95Ms: 145, ...overrides });
  return { result: "complete", baseline, soak: media("soak"), stability: [
    { unitId: "stability:decoder-fault-recovery", result: "complete", recoveryMs: 500 },
    { unitId: "stability:sharp-viewport-2x", result: "complete" },
    { unitId: "stability:adaptive-bandwidth-staircase", result: "complete", dwells: [
      "8m-1", "4m-1", "2m", "4m-2", "8m-2",
    ].map(suffix => ({ unitId: `stability:adaptive-bandwidth:${suffix}`, result: "complete" })) },
    media("stability:wss-gaming"), media("stability:webrtc-gaming-unshaped", { transport: "webrtc", latencyP95Ms: 130 }),
    media("stability:webrtc-gaming-relay-8m", { transport: "webrtc", latencyP95Ms: 130 }),
    media("stability:h264-fallback-smoke", { codec: "h264" }),
    { unitId: "stability:post-cleanup-idle", result: "complete", seconds: 60, sampleCount: 60,
      telemetryAvailability: { process: true, gpu: true, encoder: true }, gpuEncoderMeanPct: 1,
      gpuUtilizationMeanPct: 4, privateMemoryPeakMb: 100, hostMemoryDeltaMb: 5,
      cleanupEvidence: { evidenceAvailability: { service: true, displays: true, qos: true, chrome: true },
        serviceStopped: true, transientVddAbsent: true, qosPoliciesAbsent: true, chromeChildrenExited: true } },
  ] };
}
