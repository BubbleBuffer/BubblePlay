import test from "node:test";
import assert from "node:assert/strict";

import {
  STREAM_LAB_CAMPAIGN_SCHEMA_VERSION,
  aggregateParityVector,
  buildAbbaOrder,
  createCampaignStateV2,
  detectControlledDegradation,
  evaluateParetoAcceptance,
  generateArtifactRefinementConfigurations,
  generatePairwiseEncoderConfigurations,
  ratchetGates,
  selectParetoFrontier,
  summarizeSourceParityLoss,
  validateCampaignStateV2,
} from "../tools/lab/stream-lab-parity.mjs";

test("source parity summary states visual loss directly instead of implying exact-frame identity is quality", () => {
  const loss = summarizeSourceParityLoss(parityVector({
    vmafMean: 79.6, vmafP5: 43.8, ssimMean: 0.95, ssimP5: 0.895,
    edgeFidelity: 0.68, exactFrameRatio: 1, ocrCer: 0.56,
    colorError: 2.94, blockArtifactSeverity: 31,
  }));
  assert.equal(loss.status, "measured-loss");
  assert.equal(loss.vmafMeanPointsLost, 20.4);
  assert.equal(loss.vmafP5PointsLost, 56.2);
  assert.equal(loss.edgeFidelityLost, 0.32);
  assert.equal(loss.frameIdentityMismatchRate, 0);
  assert.equal(loss.ocrCharacterErrorRate, 0.56);
});

test("parity aggregation preserves poor-frame, localized, temporal, and cadence evidence", () => {
  const vector = aggregateParityVector([
    paritySample({ sourceFrameId: 10, vmaf: 95, ssim: 0.98, worstTileSsim: 0.92, presentedAtMs: 0 }),
    paritySample({ sourceFrameId: 11, vmaf: 70, ssim: 0.80, worstTileSsim: 0.30, presentedAtMs: 50,
      edgeFidelity: 0.60, colorError: 8, unchangedRegionError: 0.2, blockArtifactSeverity: 0.3 }),
    paritySample({ sourceFrameId: 11, vmaf: 90, ssim: 0.95, worstTileSsim: 0.85, presentedAtMs: 100 }),
    paritySample({ sourceFrameId: 13, vmaf: 92, ssim: 0.96, worstTileSsim: 0.88, presentedAtMs: 250 }),
  ], { expectedFrameIds: [10, 11, 12, 13] });

  assert.equal(vector.frameCount, 4);
  assert.equal(vector.vmafP5, 70);
  assert.equal(vector.ssimP5, 0.8);
  assert.equal(vector.worstTileSsim, 0.3);
  assert.equal(vector.duplicateFrames, 1);
  assert.equal(vector.droppedFrames, 1);
  assert.equal(vector.freezeCount, 1);
  assert.equal(vector.exactFrameRatio, 3 / 4);
  assert.equal(vector.presentationCadenceP95Ms, 150);
});

test("controlled degradations are detected by independent parity dimensions", () => {
  const baseline = parityVector();
  const cases = {
    blur: { ...baseline, edgeFidelity: 0.7 },
    "color-shift": { ...baseline, colorError: 6 },
    "localized-corruption": { ...baseline, worstTileSsim: 0.6 },
    "stale-region": { ...baseline, unchangedRegionError: 0.15 },
    duplicate: { ...baseline, duplicateFrames: 2 },
    drop: { ...baseline, droppedFrames: 2 },
    "cadence-stall": { ...baseline, presentationCadenceP95Ms: 120, freezeCount: 1 },
  };
  for (const [kind, degraded] of Object.entries(cases)) {
    const result = detectControlledDegradation(kind, baseline, degraded, { relativeFloor: 0.05 });
    assert.equal(result.detected, true, `${kind}: ${result.reasons.join(",")}`);
    assert.ok(result.signals.length >= 1);
  }
  assert.equal(detectControlledDegradation("blur", baseline, baseline).detected, false);
});

test("Pareto acceptance is noise-aware and permits measured efficiency wins", () => {
  const baseline = parityVector({ id: "production", wireBitrateMbps: 4, latencyP95Ms: 180 });
  const quality = parityVector({ id: "quality", vmafP5: 92.5, wireBitrateMbps: 4.1, latencyP95Ms: 184 });
  const qualityResult = evaluateParetoAcceptance(baseline, quality, {
    noise: { vmafP5: 0.5, wireBitrateMbps: 0.05, latencyP95Ms: 3 },
  });
  assert.equal(qualityResult.accepted, true);
  assert.equal(qualityResult.basis, "quality");

  const efficient = parityVector({ id: "efficient", wireBitrateMbps: 3.55, latencyP95Ms: 181 });
  const efficiencyResult = evaluateParetoAcceptance(baseline, efficient, {
    noise: { vmafP5: 0.2, wireBitrateMbps: 0.02, latencyP95Ms: 2 },
  });
  assert.equal(efficiencyResult.accepted, true);
  assert.equal(efficiencyResult.basis, "efficiency");

  const qualityPrimaryResult = evaluateParetoAcceptance(baseline, efficient, {
    noise: { vmafP5: 0.2, wireBitrateMbps: 0.02, latencyP95Ms: 2 },
    qualityPrimary: true,
  });
  assert.equal(qualityPrimaryResult.accepted, false);
  assert.ok(qualityPrimaryResult.reasons.includes("no-material-quality-improvement"));

  const regressed = parityVector({ id: "regressed", wireBitrateMbps: 3.4, worstTileSsim: 0.80 });
  const rejected = evaluateParetoAcceptance(baseline, regressed, { noise: { worstTileSsim: 0.01 } });
  assert.equal(rejected.accepted, false);
  assert.ok(rejected.reasons.includes("worstTileSsim-regression"));

  const immaterial = parityVector({ id: "rounding", vmafP5: baseline.vmafP5 + 0.01 });
  const unchanged = evaluateParetoAcceptance(baseline, immaterial, { noise: {} });
  assert.equal(unchanged.accepted, false);
  assert.ok(unchanged.reasons.includes("no-material-improvement"));
});

test("frontier removes dominated candidates and gate ratchets only tighten", () => {
  const candidates = [
    parityVector({ id: "a", vmafP5: 92, wireBitrateMbps: 4, latencyP95Ms: 170 }),
    parityVector({ id: "b", vmafP5: 91, wireBitrateMbps: 5, latencyP95Ms: 190 }),
    parityVector({ id: "c", vmafP5: 91, wireBitrateMbps: 3, latencyP95Ms: 180 }),
  ];
  assert.deepEqual(selectParetoFrontier(candidates).map(item => item.id), ["a", "c"]);

  const gates = ratchetGates({ qualityFloors: { vmafP5: 90 }, ceilings: { latencyP95Ms: 200, wireBitrateMbps: 5 } },
    parityVector({ vmafP5: 92, latencyP95Ms: 180, wireBitrateMbps: 4 }));
  assert.deepEqual(gates, {
    qualityFloors: { vmafP5: 92 },
    ceilings: { latencyP95Ms: 180, wireBitrateMbps: 4 },
  });
  assert.deepEqual(ratchetGates(gates, parityVector({ vmafP5: 91, latencyP95Ms: 190, wireBitrateMbps: 4.5 })), gates);
});

test("encoder design is deterministic, bounded, broad, and mode-constrained", () => {
  const work = generatePairwiseEncoderConfigurations({ mode: "work" });
  const repeat = generatePairwiseEncoderConfigurations({ mode: "work" });
  const game = generatePairwiseEncoderConfigurations({ mode: "game" });
  assert.equal(work.length, 96);
  assert.deepEqual(work, repeat);
  assert.deepEqual(new Set(work.map(item => item.preset)), new Set(["p1", "p2", "p3", "p4", "p5", "p6", "p7"]));
  assert.deepEqual(new Set(work.map(item => item.multipass)), new Set(["disabled", "quarter", "full"]));
  assert.deepEqual(new Set(work.map(item => item.aqMode)), new Set(["none", "spatial", "temporal"]));
  assert.ok(work.every(item => item.lookaheadFrames >= 0 && item.lookaheadFrames <= 2));
  assert.ok(game.every(item => item.lookaheadFrames >= 0 && item.lookaheadFrames <= 1));
  assert.ok(work.every((item, index) => item.experimentId === `enc-work-${String(index + 1).padStart(3, "0")}`));
  assert.ok(work.every(item => item.implementationVariant === "candidate"));
  assert.ok(work.every(item => item.aqMode === "none" ? item.aqStrength === 1 : item.aqStrength >= 1 && item.aqStrength <= 15));
  assertPairwiseCoverage(work, [
    "preset", "multipass", "aqMode", "aqStrength", "vbvFrames", "lookaheadFrames",
    "weightedPrediction", "gopSeconds", "bitrateMbps",
  ]);
});

test("artifact refinement design is a deterministic focused neighborhood", () => {
  const configurations = generateArtifactRefinementConfigurations();
  assert.equal(configurations.length, 16);
  assert.deepEqual(configurations, generateArtifactRefinementConfigurations());
  assert.ok(configurations.every((configuration, index) =>
    configuration.experimentId === `refine-artifact-${String(index + 1).padStart(3, "0")}`));
  assert.ok(configurations.every(configuration => configuration.multipass === "quarter"));
  assert.ok(configurations.every(configuration => configuration.lookaheadFrames === 0));
  assert.ok(configurations.every(configuration => configuration.bitrateMbps === 12));
  assert.deepEqual(new Set(configurations.map(configuration => configuration.preset)),
    new Set(["p1", "p2", "p3", "p4"]));
  assert.deepEqual(new Set(configurations.map(configuration => configuration.aqMode)),
    new Set(["none", "spatial"]));
  assert.deepEqual(new Set(configurations.filter(configuration => configuration.aqMode === "spatial")
    .map(configuration => configuration.aqStrength)), new Set([2, 4, 6]));
  assert.deepEqual(new Set(configurations.map(configuration => configuration.vbvFrames)), new Set([3, 4]));
  assert.deepEqual(new Set(configurations.map(configuration => configuration.weightedPrediction)), new Set([false, true]));
  assert.deepEqual(new Set(configurations.map(configuration => configuration.gopSeconds)), new Set([1, 2]));
});

test("ABBA ordering alternates pair orientation across repetitions", () => {
  assert.deepEqual(buildAbbaOrder("production", "candidate", 2), [
    { repetition: 1, position: 1, variant: "production" },
    { repetition: 1, position: 2, variant: "candidate" },
    { repetition: 1, position: 3, variant: "candidate" },
    { repetition: 1, position: 4, variant: "production" },
    { repetition: 2, position: 1, variant: "candidate" },
    { repetition: 2, position: 2, variant: "production" },
    { repetition: 2, position: 3, variant: "production" },
    { repetition: 2, position: 4, variant: "candidate" },
  ]);
});

test("campaign state v2 defaults are resumable and validation fails closed", () => {
  const state = createCampaignStateV2({ runId: "run-469", now: "2026-08-24T10:00:00.000Z" });
  assert.equal(state.schemaVersion, STREAM_LAB_CAMPAIGN_SCHEMA_VERSION);
  assert.equal(state.status, "running");
  assert.deepEqual(state.checkpoint, { stage: 0, epoch: 0, unitId: null, phase: "preflight" });
  assert.deepEqual(state.frontier, []);
  assert.deepEqual(validateCampaignStateV2(state), { valid: true, reasons: [] });

  const invalid = structuredClone(state);
  invalid.schemaVersion = 1;
  invalid.checkpoint.stage = -1;
  invalid.frontier = [{ id: "x" }, { id: "x" }];
  invalid.pauseState.paused = "yes";
  const result = validateCampaignStateV2(invalid);
  assert.equal(result.valid, false);
  assert.ok(result.reasons.includes("unsupported-schema-version"));
  assert.ok(result.reasons.includes("invalid-checkpoint"));
  assert.ok(result.reasons.includes("duplicate-frontier-id"));
  assert.ok(result.reasons.includes("invalid-pause-state"));
});

function paritySample(overrides = {}) {
  return {
    sourceFrameId: 1,
    exactFrame: true,
    vmaf: 95,
    ssim: 0.98,
    worstTileSsim: 0.92,
    edgeFidelity: 0.95,
    colorError: 1,
    ocrCer: 0.01,
    unchangedRegionError: 0.01,
    blockArtifactSeverity: 0.01,
    presentedAtMs: 0,
    ...overrides,
  };
}

function parityVector(overrides = {}) {
  return {
    id: "candidate",
    vmafMean: 95,
    vmafP5: 90,
    ssimMean: 0.97,
    ssimP5: 0.94,
    worstTileSsim: 0.9,
    edgeFidelity: 0.94,
    colorError: 1,
    ocrCer: 0.01,
    unchangedRegionError: 0.01,
    blockArtifactSeverity: 0.01,
    exactFrameRatio: 1,
    duplicateFrames: 0,
    droppedFrames: 0,
    freezeCount: 0,
    presentationCadenceP95Ms: 34,
    wireBitrateMbps: 4,
    latencyP95Ms: 180,
    ...overrides,
  };
}

function assertPairwiseCoverage(rows, fields) {
  for (let left = 0; left < fields.length; left++) for (let right = left + 1; right < fields.length; right++) {
    const leftField = fields[left], rightField = fields[right];
    const leftValues = [...new Set(rows.map(row => row[leftField]))];
    const rightValues = [...new Set(rows.map(row => row[rightField]))];
    const observed = new Set(rows.map(row => `${row[leftField]}|${row[rightField]}`));
    for (const leftValue of leftValues) for (const rightValue of rightValues) {
      const conditionallyInvalid = leftField === "aqMode" && rightField === "aqStrength" &&
        leftValue === "none" && rightValue !== 1;
      if (!conditionallyInvalid) assert.ok(observed.has(`${leftValue}|${rightValue}`),
        `missing pair ${leftField}=${leftValue}, ${rightField}=${rightValue}`);
    }
  }
}
