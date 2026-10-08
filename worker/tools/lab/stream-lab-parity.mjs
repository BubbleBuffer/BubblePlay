import { percentile } from "./stream-lab-core.mjs";

export const STREAM_LAB_CAMPAIGN_SCHEMA_VERSION = 2;

const HIGHER_IS_BETTER = Object.freeze([
  "vmafMean", "vmafP5", "ssimMean", "ssimP5", "worstTileSsim", "edgeFidelity", "exactFrameRatio",
]);
const LOWER_IS_BETTER = Object.freeze([
  "colorError", "ocrCer", "unchangedRegionError", "blockArtifactSeverity", "duplicateFrames", "droppedFrames",
  "freezeCount", "presentationCadenceP95Ms", "wireBitrateMbps", "latencyP95Ms",
]);
const PARITY_QUALITY_FIELDS = Object.freeze([
  ...HIGHER_IS_BETTER.filter(name => name !== "exactFrameRatio"),
  "colorError", "ocrCer", "unchangedRegionError", "blockArtifactSeverity", "exactFrameRatio",
  "duplicateFrames", "droppedFrames", "freezeCount", "presentationCadenceP95Ms",
]);
const MATERIAL_DELTA = Object.freeze({
  vmafMean: 2, vmafP5: 2,
  ssimMean: 0.01, ssimP5: 0.01, worstTileSsim: 0.01, edgeFidelity: 0.01,
  colorError: 0.5, ocrCer: 0.01, unchangedRegionError: 0.01, blockArtifactSeverity: 0.01,
  exactFrameRatio: 0.01, duplicateFrames: 1, droppedFrames: 1, freezeCount: 1,
  presentationCadenceP95Ms: 5,
});

export function aggregateParityVector(samples, options = {}) {
  if (!Array.isArray(samples) || samples.length === 0) throw new TypeError("parity samples must be a non-empty array");
  const numeric = (name) => samples.map(sample => sample?.[name]).filter(Number.isFinite);
  const mean = (name) => average(numeric(name));
  const minimum = (name) => minOrNull(numeric(name));
  const sourceFrameIds = samples.map(sample => sample?.sourceFrameId).filter(Number.isSafeInteger);
  const uniqueFrameIds = new Set(sourceFrameIds);
  const expectedFrameIds = Array.isArray(options.expectedFrameIds)
    ? [...new Set(options.expectedFrameIds.filter(Number.isSafeInteger))]
    : inferredFrameRange(sourceFrameIds);
  const expectedSet = new Set(expectedFrameIds);
  const exactCount = expectedFrameIds.length
    ? [...uniqueFrameIds].filter(id => expectedSet.has(id)).length
    : samples.filter(sample => sample?.exactFrame === true).length;
  const presentationTimes = numeric("presentedAtMs").sort((a, b) => a - b);
  const cadence = presentationTimes.slice(1).map((time, index) => time - presentationTimes[index]);
  const freezeThresholdMs = options.freezeThresholdMs ?? 100;
  return {
    frameCount: samples.length,
    vmafMean: mean("vmaf"),
    vmafP5: percentile(numeric("vmaf"), 0.05),
    ssimMean: mean("ssim"),
    ssimP5: percentile(numeric("ssim"), 0.05),
    worstTileSsim: minimum("worstTileSsim"),
    edgeFidelity: mean("edgeFidelity"),
    colorError: mean("colorError"),
    ocrCer: mean("ocrCer"),
    unchangedRegionError: mean("unchangedRegionError"),
    blockArtifactSeverity: mean("blockArtifactSeverity"),
    exactFrameRatio: expectedFrameIds.length ? exactCount / expectedFrameIds.length : exactCount / samples.length,
    duplicateFrames: sourceFrameIds.length - uniqueFrameIds.size,
    droppedFrames: expectedFrameIds.filter(id => !uniqueFrameIds.has(id)).length,
    freezeCount: cadence.filter(interval => interval > freezeThresholdMs).length,
    presentationCadenceP95Ms: percentile(cadence, 0.95),
  };
}

export function detectControlledDegradation(kind, baseline, degraded, options = {}) {
  const noise = options.noise ?? {};
  const relativeFloor = options.relativeFloor ?? 0.02;
  const checks = {
    blur: [["edgeFidelity", "lower"], ["vmafP5", "lower"]],
    "color-shift": [["colorError", "higher"]],
    "localized-corruption": [["worstTileSsim", "lower"], ["blockArtifactSeverity", "higher"]],
    "stale-region": [["unchangedRegionError", "higher"]],
    duplicate: [["duplicateFrames", "higher"]],
    drop: [["droppedFrames", "higher"], ["exactFrameRatio", "lower"]],
    "cadence-stall": [["presentationCadenceP95Ms", "higher"], ["freezeCount", "higher"]],
  }[kind];
  if (!checks) throw new RangeError(`unknown controlled degradation: ${kind}`);
  const signals = [];
  const reasons = [];
  for (const [field, direction] of checks) {
    const before = baseline?.[field], after = degraded?.[field];
    if (!Number.isFinite(before) || !Number.isFinite(after)) {
      reasons.push(`${field}-unavailable`);
      continue;
    }
    const threshold = Math.max(2 * finiteOr(noise[field], 0), Math.abs(before) * relativeFloor,
      Number.isInteger(before) && Number.isInteger(after) ? 0.5 : 1e-9);
    const delta = after - before;
    if ((direction === "higher" && delta >= threshold) || (direction === "lower" && -delta >= threshold)) {
      signals.push({ field, direction, before, after, delta });
    } else reasons.push(`${field}-not-detected`);
  }
  return { kind, detected: signals.length > 0, signals, reasons };
}

export function evaluateParetoAcceptance(baseline, candidate, options = {}) {
  const noise = options.noise ?? {};
  const reasons = [];
  const improvements = [];
  for (const field of PARITY_QUALITY_FIELDS) {
    const before = baseline?.[field], after = candidate?.[field];
    if (!Number.isFinite(before) || !Number.isFinite(after)) {
      reasons.push(`${field}-unavailable`);
      continue;
    }
    const allowance = 2 * finiteOr(noise[field], 0);
    const materialDelta = Math.max(allowance, MATERIAL_DELTA[field] ?? 0);
    const delta = after - before;
    const higher = HIGHER_IS_BETTER.includes(field);
    if ((higher && delta < -allowance) || (!higher && delta > allowance)) reasons.push(`${field}-regression`);
    if ((higher && delta >= materialDelta) || (!higher && delta <= -materialDelta)) improvements.push(field);
  }
  reasons.push(...gateViolationReasons(candidate, options.gates));
  const bitrateReduction = fractionalReduction(baseline?.wireBitrateMbps, candidate?.wireBitrateMbps);
  const latencyReductionMs = Number.isFinite(baseline?.latencyP95Ms) && Number.isFinite(candidate?.latencyP95Ms)
    ? baseline.latencyP95Ms - candidate.latencyP95Ms : null;
  const efficiencyWin = bitrateReduction >= 0.10 || latencyReductionMs >= 10;
  const qualityWin = improvements.length > 0;
  if (options.qualityPrimary === true && !qualityWin) reasons.push("no-material-quality-improvement");
  else if (!qualityWin && !efficiencyWin) reasons.push("no-material-improvement");
  return {
    accepted: reasons.length === 0,
    basis: reasons.length ? null : qualityWin ? "quality" : "efficiency",
    improvements,
    efficiency: { bitrateReduction, latencyReductionMs },
    reasons: [...new Set(reasons)],
  };
}

export function summarizeSourceParityLoss(metrics = {}) {
  const loss = {
    vmafMeanPointsLost: gapFrom(metrics.vmafMean, 100),
    vmafP5PointsLost: gapFrom(metrics.vmafP5, 100),
    ssimMeanLost: gapFrom(metrics.ssimMean, 1),
    ssimP5Lost: gapFrom(metrics.ssimP5, 1),
    edgeFidelityLost: gapFrom(metrics.edgeFidelity, 1),
    frameIdentityMismatchRate: gapFrom(metrics.exactFrameRatio, 1),
    ocrCharacterErrorRate: finiteOrNull(metrics.ocrCer),
    colorError: finiteOrNull(metrics.colorError),
    blockArtifactSeverity: finiteOrNull(metrics.blockArtifactSeverity),
  };
  const available = Object.values(loss).filter(Number.isFinite);
  return {
    status: available.length === 0 ? "unavailable" : available.some(value => value > 1e-9) ? "measured-loss" : "source-parity",
    ...loss,
  };
}

export function selectParetoFrontier(candidates, options = {}) {
  if (!Array.isArray(candidates)) throw new TypeError("candidates must be an array");
  const noise = options.noise ?? {};
  return candidates.filter((candidate, index) => !candidates.some((other, otherIndex) =>
    index !== otherIndex && dominates(other, candidate, noise)));
}

export function ratchetGates(current, champion) {
  const qualityFloors = { ...(current?.qualityFloors ?? {}) };
  const ceilings = { ...(current?.ceilings ?? {}) };
  for (const field of Object.keys(qualityFloors)) {
    if (Number.isFinite(champion?.[field])) qualityFloors[field] = Math.max(qualityFloors[field], champion[field]);
  }
  for (const field of Object.keys(ceilings)) {
    if (Number.isFinite(champion?.[field])) ceilings[field] = Math.min(ceilings[field], champion[field]);
  }
  return { qualityFloors, ceilings };
}

export function generatePairwiseEncoderConfigurations(options = {}) {
  const mode = options.mode ?? "work";
  const count = options.count ?? 96;
  if (!['work', 'game'].includes(mode)) throw new RangeError("mode must be work or game");
  if (!Number.isInteger(count) || count < 1 || count > 512) throw new RangeError("count must be an integer from 1 to 512");
  const factors = [
    ["preset", ["p1", "p2", "p3", "p4", "p5", "p6", "p7"]],
    ["multipass", ["disabled", "quarter", "full"]],
    ["aqMode", ["none", "spatial", "temporal"]],
    ["aqStrength", [1, 4, 8, 12, 15]],
    ["vbvFrames", [1, 2, 3, 4]],
    ["lookaheadFrames", mode === "work" ? [0, 1, 2] : [0, 1]],
    ["weightedPrediction", [false, true]],
    ["gopSeconds", [1, 2, 4]],
    ["bitrateMbps", [2, 4, 8, 12]],
  ];
  const pool = buildCandidatePool(factors);
  const uncovered = new Set(pool.flatMap(configuration => pairKeys(configuration, factors)));
  const selected = [];
  const remaining = [...pool];
  while (selected.length < count && remaining.length) {
    let bestIndex = 0, bestScore = -1;
    for (let index = 0; index < remaining.length; index++) {
      const score = pairKeys(remaining[index], factors).reduce((sum, key) => sum + Number(uncovered.has(key)), 0);
      if (score > bestScore) { bestScore = score; bestIndex = index; }
    }
    const [best] = remaining.splice(bestIndex, 1);
    selected.push(best);
    for (const key of pairKeys(best, factors)) uncovered.delete(key);
  }
  return selected.map((configuration, index) => ({
    ...configuration,
    tuning: "ultra-low-latency",
    implementationVariant: "candidate",
    experimentId: `enc-${mode}-${String(index + 1).padStart(3, "0")}`,
  }));
}

export function generateArtifactRefinementConfigurations() {
  const neighborhood = [
    ["p2", "none", 1, 4, true, 2],
    ["p1", "none", 1, 4, true, 2],
    ["p3", "none", 1, 4, true, 2],
    ["p4", "none", 1, 4, true, 2],
    ["p2", "spatial", 2, 4, true, 2],
    ["p2", "spatial", 4, 4, true, 2],
    ["p2", "spatial", 6, 4, true, 2],
    ["p1", "spatial", 2, 3, false, 1],
    ["p1", "spatial", 4, 4, true, 2],
    ["p1", "spatial", 6, 3, true, 1],
    ["p3", "spatial", 2, 4, false, 1],
    ["p3", "spatial", 4, 3, true, 2],
    ["p3", "spatial", 6, 4, true, 1],
    ["p4", "spatial", 2, 3, true, 2],
    ["p4", "spatial", 4, 4, false, 1],
    ["p4", "spatial", 6, 3, false, 2],
  ];
  return neighborhood.map(([preset, aqMode, aqStrength, vbvFrames, weightedPrediction, gopSeconds], index) => ({
    preset,
    tuning: "ultra-low-latency",
    multipass: "quarter",
    aqMode,
    aqStrength,
    vbvFrames,
    lookaheadFrames: 0,
    weightedPrediction,
    gopSeconds,
    bitrateMbps: 12,
    implementationVariant: "candidate",
    experimentId: `refine-artifact-${String(index + 1).padStart(3, "0")}`,
  }));
}

export function buildAbbaOrder(production = "production", candidate = "candidate", repetitions = 1) {
  if (!Number.isInteger(repetitions) || repetitions < 1 || repetitions > 100) throw new RangeError("repetitions must be from 1 to 100");
  const result = [];
  for (let repetition = 1; repetition <= repetitions; repetition++) {
    const [a, b] = repetition % 2 ? [production, candidate] : [candidate, production];
    [a, b, b, a].forEach((variant, index) => result.push({ repetition, position: index + 1, variant }));
  }
  return result;
}

export function createCampaignStateV2({ runId, now = new Date().toISOString(), gates } = {}) {
  if (!validIdentifier(runId, 120)) throw new TypeError("invalid campaign runId");
  if (!validIsoDate(now)) throw new TypeError("invalid campaign timestamp");
  return {
    schemaVersion: STREAM_LAB_CAMPAIGN_SCHEMA_VERSION,
    runId,
    status: "running",
    createdAt: now,
    updatedAt: now,
    checkpoint: { stage: 0, epoch: 0, unitId: null, phase: "preflight" },
    pauseState: { paused: false, reason: null, since: null, resumeAfter: null },
    frontier: [],
    gates: gates ? structuredClone(gates) : { qualityFloors: {}, ceilings: {} },
    hypotheses: [],
    researchEvidence: [],
    rejectedVariants: [],
    acceptedVariants: [],
    plateau: { consecutiveFailedHypotheses: 0, baselineConfirmed: false },
    units: {},
  };
}

export function validateCampaignStateV2(state) {
  const reasons = [];
  if (!state || typeof state !== "object" || Array.isArray(state)) return { valid: false, reasons: ["invalid-state"] };
  if (state.schemaVersion !== STREAM_LAB_CAMPAIGN_SCHEMA_VERSION) reasons.push("unsupported-schema-version");
  if (!validIdentifier(state.runId, 120)) reasons.push("invalid-run-id");
  if (!["running", "paused", "complete", "failed", "plateau"].includes(state.status)) reasons.push("invalid-status");
  if (!validIsoDate(state.createdAt) || !validIsoDate(state.updatedAt)) reasons.push("invalid-timestamp");
  const pause = state.pauseState;
  if (!pause || typeof pause.paused !== "boolean" || !validNullableText(pause.reason, 200) ||
      !validNullableIsoDate(pause.since) || !validNullableIsoDate(pause.resumeAfter) ||
      (pause.paused && (!pause.reason || !pause.since)) || (state.status === "paused") !== pause.paused) {
    reasons.push("invalid-pause-state");
  }
  const checkpoint = state.checkpoint;
  if (!checkpoint || !Number.isInteger(checkpoint.stage) || checkpoint.stage < 0 || checkpoint.stage > 4 ||
      !Number.isInteger(checkpoint.epoch) || checkpoint.epoch < 0 || !validNullableIdentifier(checkpoint.unitId, 160) ||
      !validIdentifier(checkpoint.phase, 80)) reasons.push("invalid-checkpoint");
  if (!Array.isArray(state.frontier)) reasons.push("invalid-frontier");
  else {
    const ids = state.frontier.map(item => item?.id);
    if (ids.some(id => !validIdentifier(id, 120))) reasons.push("invalid-frontier-id");
    if (new Set(ids).size !== ids.length) reasons.push("duplicate-frontier-id");
  }
  for (const name of ["hypotheses", "researchEvidence", "rejectedVariants", "acceptedVariants"]) {
    if (!Array.isArray(state[name])) reasons.push(`invalid-${name}`);
  }
  if (!state.gates || typeof state.gates.qualityFloors !== "object" || typeof state.gates.ceilings !== "object") reasons.push("invalid-gates");
  if (!state.plateau || !Number.isInteger(state.plateau.consecutiveFailedHypotheses) ||
      state.plateau.consecutiveFailedHypotheses < 0 || typeof state.plateau.baselineConfirmed !== "boolean") reasons.push("invalid-plateau");
  if (!state.units || typeof state.units !== "object" || Array.isArray(state.units)) reasons.push("invalid-units");
  return { valid: reasons.length === 0, reasons: [...new Set(reasons)] };
}

function inferredFrameRange(ids) {
  if (!ids.length) return [];
  const start = Math.min(...ids), end = Math.max(...ids);
  if (end - start > 1_000_000) return [];
  return Array.from({ length: end - start + 1 }, (_, index) => start + index);
}

function dominates(left, right, noise) {
  let strictlyBetter = false;
  for (const field of [...HIGHER_IS_BETTER, ...LOWER_IS_BETTER]) {
    if (!Number.isFinite(left?.[field]) || !Number.isFinite(right?.[field])) continue;
    const allowance = finiteOr(noise[field], 0);
    const delta = left[field] - right[field];
    if (HIGHER_IS_BETTER.includes(field)) {
      if (delta < -allowance) return false;
      if (delta > allowance) strictlyBetter = true;
    } else {
      if (delta > allowance) return false;
      if (delta < -allowance) strictlyBetter = true;
    }
  }
  return strictlyBetter;
}

function gateViolationReasons(candidate, gates) {
  if (!gates) return [];
  const reasons = [];
  for (const [field, floor] of Object.entries(gates.qualityFloors ?? {})) {
    if (!Number.isFinite(candidate?.[field]) || candidate[field] < floor) reasons.push(`${field}-below-ratcheted-floor`);
  }
  for (const [field, ceiling] of Object.entries(gates.ceilings ?? {})) {
    if (!Number.isFinite(candidate?.[field]) || candidate[field] > ceiling) reasons.push(`${field}-above-ratcheted-ceiling`);
  }
  return reasons;
}

function buildCandidatePool(factors) {
  const bySignature = new Map();
  const add = (values) => {
    const candidate = Object.fromEntries(factors.map(([name, levels], index) => [name, values[index] ?? levels[0]]));
    if (candidate.aqMode === "none") candidate.aqStrength = 1;
    const signature = factors.map(([name]) => String(candidate[name])).join("|");
    if (!bySignature.has(signature)) bySignature.set(signature, candidate);
  };
  for (let left = 0; left < factors.length; left++) for (let right = left + 1; right < factors.length; right++) {
    for (let a = 0; a < factors[left][1].length; a++) for (let b = 0; b < factors[right][1].length; b++) {
      for (let salt = 0; salt < 4; salt++) {
        const values = factors.map(([, levels], index) => levels[(a * 17 + b * 11 + salt * 7 + index * 5) % levels.length]);
        values[left] = factors[left][1][a];
        values[right] = factors[right][1][b];
        add(values);
      }
    }
  }
  return [...bySignature.values()];
}

function pairKeys(configuration, factors) {
  const keys = [];
  for (let left = 0; left < factors.length; left++) for (let right = left + 1; right < factors.length; right++) {
    const leftName = factors[left][0], rightName = factors[right][0];
    keys.push(`${leftName}=${configuration[leftName]}&${rightName}=${configuration[rightName]}`);
  }
  return keys;
}

function fractionalReduction(before, after) {
  return Number.isFinite(before) && before > 0 && Number.isFinite(after) ? (before - after) / before : null;
}

function average(values) {
  return values.length ? values.reduce((sum, value) => sum + value, 0) / values.length : null;
}

function minOrNull(values) {
  return values.length ? Math.min(...values) : null;
}

function finiteOr(value, fallback) {
  return Number.isFinite(value) ? value : fallback;
}

function finiteOrNull(value) {
  return Number.isFinite(value) ? value : null;
}

function gapFrom(value, ideal) {
  return Number.isFinite(value) ? Math.round(Math.max(0, ideal - value) * 1e6) / 1e6 : null;
}

function validIdentifier(value, maximum) {
  return typeof value === "string" && value.length > 0 && value.length <= maximum && /^[A-Za-z0-9][A-Za-z0-9._:-]*$/.test(value);
}

function validNullableIdentifier(value, maximum) {
  return value === null || validIdentifier(value, maximum);
}

function validIsoDate(value) {
  return typeof value === "string" && Number.isFinite(Date.parse(value));
}

function validNullableIsoDate(value) {
  return value === null || validIsoDate(value);
}

function validNullableText(value, maximum) {
  return value === null || (typeof value === "string" && value.length > 0 && value.length <= maximum);
}
