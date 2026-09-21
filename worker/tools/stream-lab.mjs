#!/usr/bin/env node
import { existsSync, mkdirSync, readFileSync, readdirSync, rmSync, statSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { createHash, randomUUID } from "node:crypto";
import { spawnSync } from "node:child_process";

import { defaultActivityGuardPath, launchPhysicalActivityGuard } from "./stream-lab-activity.mjs";
import { runOptimizationLoop } from "./stream-lab-optimize.mjs";
import { createCampaignStateV2, validateCampaignStateV2 } from "./stream-lab-parity.mjs";
import { STREAM_LAB_COMPATIBILITY } from "../public/media.js";

import {
  SCENES,
  artifactStore,
  compareCandidateFiles,
  evaluateCampaignAcceptance,
} from "./stream-lab-core.mjs";
import {
  STREAM_LAB_PIPE,
  buildEncoderRound,
  buildHelperRequest,
  findChrome,
  inspectLabService,
  persistentReceiverProfile,
  sendHelperRequest,
  validateRunId,
} from "./stream-lab-runtime.mjs";

export {
  STREAM_LAB_PIPE,
  buildEncoderRound,
  buildHelperRequest,
  findChrome,
  inspectLabService,
  persistentReceiverProfile,
  sendHelperRequest,
} from "./stream-lab-runtime.mjs";

const workerRoot = resolve(import.meta.dirname, "..");
const projectRoot = resolve(workerRoot, "..");
const QUALITY_SCENE_IDS = new Set([
  ...SCENES.map(scene => scene.id),
  "source-code", "chrome-text", "chrome-canvas", "generated-video",
]);

export function runPreflight(options = {}) {
  const env = options.env ?? process.env;
  const commandProbe = options.commandProbe ?? probeCommand;
  const chromeFinder = options.chromeFinder ?? findChrome;
  const receiverProfile = persistentReceiverProfile(env);
  mkdirSync(receiverProfile, { recursive: true });
  const dependencies = [
    { name: "node", ...commandProbe(process.execPath, ["--version"]) },
    { name: "cargo", ...commandProbe("cargo", ["--version"]) },
    { name: "ffmpeg-libvmaf", ...commandProbe("ffmpeg", ["-hide_banner", "-filters"], output => /\blibvmaf\b/.test(output)) },
    { name: "ffprobe", ...commandProbe("ffprobe", ["-version"]) },
    { name: "tesseract-eng-deu", ...commandProbe("tesseract", ["--list-langs"], output => /(^|\s)eng(\s|$)/m.test(output) && /(^|\s)deu(\s|$)/m.test(output)) },
    { name: "chromium", available: Boolean(chromeFinder(env)), version: chromeFinder(env) ?? null },
  ];
  const labService = (options.serviceProbe ?? (() => inspectLabService({ env })))();
  const optionalTelemetry = [
    { name: "nvidia-smi", ...commandProbe("nvidia-smi", ["--query-gpu=name", "--format=csv,noheader"]) },
  ];
  const missing = dependencies.filter(({ available }) => !available).map(({ name }) => name);
  // Live sessions require begin/renew/end even with a persistent display, and
  // cleanup verifies STOPPED through SCM. A reachable pipe alone is insufficient.
  if (!labService.queryable) missing.push("lab-helper-service-access");
  else if (!labService.installed) missing.push("lab-helper-service");
  const activityGuard = options.activityGuardPath ?? defaultActivityGuardPath();
  if (options.requireActivityGuard && !existsSync(activityGuard)) missing.push("physical-activity-guard");
  return {
    schemaVersion: 1,
    checkedAt: new Date().toISOString(),
    result: missing.length ? "unavailable" : "ready",
    missing,
    receiverProfile,
    labService,
    sourcePage: new URL("../public/stream-lab-source.html", import.meta.url).href,
    dependencies,
    optionalTelemetry,
    activityGuard: options.requireActivityGuard ? activityGuard : null,
  };
}

export function buildCandidateHost(options = {}) {
  const root = resolve(options.projectRoot ?? projectRoot);
  const hostRoot = join(root, "host");
  const executable = join(hostRoot, "target-candidate", "release", "bubbleplay-host.exe");
  const activityGuard = join(hostRoot, "target-candidate", "release", "bubbleplay_lab_activity_guard.exe");
  const runner = options.spawnSync ?? spawnSync;
  const result = runner("cargo", ["build", "--release", "--bin", "bubbleplay-host", "--bin",
    "bubbleplay_lab_activity_guard", "--target-dir", "target-candidate"], {
    cwd: hostRoot, env: options.env ?? process.env, encoding: "utf8", windowsHide: true,
  });
  if (result.status !== 0) {
    const detail = `${result.stderr ?? ""}\n${result.stdout ?? ""}`.trim().slice(-4_000);
    throw new Error(`Candidate host build failed${detail ? `: ${detail}` : "."}`);
  }
  if (!(options.exists ?? existsSync)(executable)) throw new Error(`Candidate host build did not produce ${executable}`);
  if (!(options.exists ?? existsSync)(activityGuard)) throw new Error(`Candidate build did not produce ${activityGuard}`);
  return executable;
}

export function selectCandidateHostForRun(args, env = process.env, prepared = {}, options = {}) {
  const explicit = option(args, "--candidate-host",
    String(env.BUBBLEPLAY_STREAM_LAB_CANDIDATE_HOST || "").trim() || null);
  if (explicit) {
    const executable = resolve(explicit);
    if (!(options.exists ?? existsSync)(executable))
      throw new Error(`Explicit candidate host '${executable}' does not exist.`);
    inspectCandidateHostCompatibility(executable, options);
    return executable;
  }
  if (prepared.candidateHost) {
    inspectCandidateHostCompatibility(prepared.candidateHost, options);
    return prepared.candidateHost;
  }
  const executable = (options.buildCandidate ?? buildCandidateHost)({ env });
  inspectCandidateHostCompatibility(executable, options);
  return executable;
}

export function inspectCandidateHostCompatibility(executable, options = {}) {
  const path = resolve(executable);
  const bytes = (options.readFile ?? readFileSync)(path);
  const expectedCompatibilityId = options.expectedCompatibilityId ?? STREAM_LAB_COMPATIBILITY.id;
  const sha256 = createHash("sha256").update(bytes).digest("hex");
  const evidence = {
    path,
    sha256,
    expectedCompatibilityId,
    compatibilityMarkerPresent: bytes.includes(Buffer.from(expectedCompatibilityId, "utf8")),
  };
  if (!evidence.compatibilityMarkerPresent) {
    const error = new Error(
      `Candidate host '${path}' does not contain the required Stream Lab compatibility generation '${expectedCompatibilityId}'.`,
    );
    error.code = "candidate-host-compatibility-mismatch";
    error.detail = evidence;
    throw error;
  }
  return evidence;
}

export function prepareLabExecution(command, env, options = {}) {
  const preflightRunner = options.preflight ?? runPreflight;
  const candidateBuilder = options.buildCandidate ?? buildCandidateHost;
  let preflight = preflightRunner({ env, requireActivityGuard: false });
  let candidateHost = null;
  if (["optimize", "lane-recovery"].includes(command) && preflight.missing.length === 0) {
    candidateHost = candidateBuilder({ env });
    preflight = preflightRunner({ env, requireActivityGuard: command === "optimize" });
  }
  return { preflight, candidateHost };
}

function probeCommand(file, args, validator = () => true) {
  const result = spawnSync(file, args, { encoding: "utf8", windowsHide: true, timeout: 10_000 });
  const output = `${result.stdout ?? ""}\n${result.stderr ?? ""}`;
  return {
    available: result.status === 0 && validator(output),
    version: firstUsefulLine(output),
  };
}

function firstUsefulLine(output) {
  return output.split(/\r?\n/).map(line => line.trim()).find(Boolean) ?? null;
}

export function encoderCampaignPlan() {
  const baseline = {
    preset: "p3", tuning: "ultra-low-latency", multipass: "disabled",
    vbvFrames: 1, gopSeconds: 2, aqStrength: 8, temporalAq: false, bitrateMbps: 6,
  };
  return {
    strategy: "adaptive-successive-halving",
    baselineRuns: 7,
    baselineMatrix: [
      { bandwidthMbps: null, repetitions: 1 }, { bandwidthMbps: 2, repetitions: 1 },
      { bandwidthMbps: 4, repetitions: 3 }, { bandwidthMbps: 8, repetitions: 1 },
      { bandwidthMbps: 12, repetitions: 1 },
    ],
    screeningSeconds: 15,
    finalistCount: 3,
    dimensions: ["presetTuning", "multipass", "vbvFrames", "gopSeconds", "aqStrength", "temporalAq", "bitrateMbps"],
    firstRound: buildEncoderRound(baseline, "presetTuning"),
    finalistBandwidthsMbps: [2, 4, 8, 12],
    networkPolicy: { encoderScreening: "unshaped", constrainedHeadroom: { wss: 0.65, webrtc: 0.90 } },
    soakSeconds: 600,
    adaptiveDwellSeconds: 120,
    postCleanupIdleSeconds: 60,
    stabilityUnits: [
      { id: "decoder-fault-recovery", transport: "wss" },
      { id: "sharp-viewport-2x", transport: "wss", zoom: 2 },
      { id: "adaptive-bandwidth-staircase", transport: "wss", adaptive: true, bandwidthsMbps: [8, 4, 2, 4, 8] },
      { id: "wss-gaming", transport: "wss", streamMode: "game", fps: 60, bitrateMbps: 8 },
      { id: "webrtc-gaming-unshaped", transport: "webrtc", streamMode: "game", bandwidthMbps: null, ice: "all" },
      { id: "webrtc-gaming-relay-8m", transport: "webrtc", streamMode: "game", bandwidthMbps: 8, ice: "relay" },
      { id: "h264-fallback-smoke", transport: "wss", codec: "h264" },
      { id: "post-cleanup-idle", seconds: 60 },
    ],
    regressionRoutes: [
      { transport: "wss", streamMode: "game", bandwidthMbps: 8 },
      { transport: "webrtc", streamMode: "game", ice: "all", bandwidthMbps: null },
      { transport: "webrtc", streamMode: "game", ice: "relay", bandwidthMbps: 8 },
      { transport: "wss", codec: "h264", smoke: true },
    ],
  };
}

export function optimizationCampaignPlan() {
  return {
    ...encoderCampaignPlan(),
    strategy: "parity-pareto-loop",
    baselineRuns: 15,
    baselineMatrix: [null, 2, 4, 8, 12].map(bandwidthMbps => ({ bandwidthMbps, repetitions: 3 })),
    parameterCandidates: 96,
    screeningBandwidthsMbps: [null, 4],
    validationBandwidthsMbps: [2, 4, 8],
    finalistBandwidthsMbps: [2, 4, 8, 12],
    finalistCount: 4,
    abbaOrder: ["production", "candidate", "candidate", "production"],
    autoPauseOnPhysicalActivity: true,
    resumeIdleSeconds: 600,
    stopAfterRejectedHypotheses: 3,
  };
}

function loadMetrics(path) {
  const candidate = statSync(path).isDirectory() ? join(path, "metrics.json") : path;
  return JSON.parse(readFileSync(candidate, "utf8"));
}

function option(args, name, fallback = null) {
  const index = args.indexOf(name);
  return index >= 0 && args[index + 1] ? args[index + 1] : fallback;
}

export function isLoopbackLabUrl(value) {
  try {
    const url = new URL(value);
    return url.protocol === "http:" && ["127.0.0.1", "localhost", "[::1]"].includes(url.hostname);
  } catch {
    return false;
  }
}

export function validateLiveRunRouting(baseUrl, workerVersionOverride, transientVdd) {
  const loopbackRelay = isLoopbackLabUrl(baseUrl);
  if (loopbackRelay && workerVersionOverride) {
    throw new Error("Loopback Stream Lab runs use the local compatibility descriptor and must omit --worker-version-id");
  }
  if (!loopbackRelay && !workerVersionOverride) {
    throw new Error(
      "Stream Lab live runs require --worker-version-id so browser, signaling, and media sockets use one immutable Worker version.",
    );
  }
  return { loopbackRelay, transientVdd };
}

export function receiverAssetInterceptionForRun(loopbackRelay) {
  return !loopbackRelay;
}

export function parseDeltaTriggerFrames(args) {
  const raw = option(args, "--delta-trigger-frames", null);
  if (raw === null) return null;
  const frames = Number(raw);
  if (!Number.isInteger(frames) || frames < 1 || frames > 16)
    throw new RangeError("--delta-trigger-frames must be an integer from 1 to 16");
  return frames;
}

export function parseSceneOptions(args) {
  const index = args.indexOf("--scenes");
  if (index < 0) return null;
  const raw = args[index + 1];
  if (!raw || raw.startsWith("--")) throw new RangeError("--scenes requires at least one quality scene");
  const scenes = [...new Set(raw.split(",").map(value => value.trim()).filter(Boolean))];
  if (!scenes.length || scenes.some(scene => !QUALITY_SCENE_IDS.has(scene)))
    throw new RangeError("--scenes contains an unknown quality scene");
  return scenes;
}

export function parseWssTopologyOptions(args) {
  const rawLanes = option(args, "--wss-lanes", null);
  if (rawLanes === null) return null;
  const laneCount = Number(rawLanes);
  const parityShards = Number(option(args, "--wss-parity", "1"));
  if (!Number.isInteger(laneCount) || laneCount < 1 || laneCount > 16)
    throw new RangeError("--wss-lanes must be an integer from 1 to 16");
  if (!Number.isInteger(parityShards) || parityShards < 0 || parityShards > 2)
    throw new RangeError("--wss-parity must be 0, 1, or 2");
  const topology = { laneCount, dataShards: laneCount - parityShards, parityShards };
  if (topology.dataShards < 1) throw new RangeError("invalid WSS topology: at least one data lane is required");
  return topology;
}

export function parseWorkerVersionOptions(args, env = process.env) {
  const cliName = option(args, "--worker-name", null);
  const cliVersion = option(args, "--worker-version-id", null);
  const envName = String(env.BUBBLEPLAY_WORKER_NAME || "").trim() || null;
  const envVersion = String(env.BUBBLEPLAY_WORKER_VERSION_ID || "").trim() || null;
  const versionId = cliVersion ?? envVersion;
  const workerName = cliName ?? envName ?? (versionId ? "playfull-cat" : null);
  if (!workerName && !versionId) return null;
  if (!workerName || !versionId)
    throw new RangeError("--worker-name and --worker-version-id must both be supplied");
  if (!/^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$/.test(workerName) ||
      !/^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(versionId))
    throw new RangeError("Worker name and version identifier must be canonical");
  return { workerName, versionId: versionId.toLowerCase() };
}

export function performanceActionProbeEnabled(args, roiCandidate, hybridCandidate) {
  return !args.includes("--no-action-probe") && !roiCandidate && !hybridCandidate;
}

export function buildScreeningCandidate(args) {
  const dirtyQpMap = args.includes("--dirty-qp-map");
  const main10 = args.includes("--main10");
  const roiIndex = args.indexOf("--roi-mode");
  const roiMode = roiIndex < 0 ? null : args[roiIndex + 1];
  if (roiIndex >= 0 && !["damage", "text-edge", "feedback", "feedback-headroom"].includes(roiMode))
    throw new RangeError("--roi-mode must be damage, text-edge, feedback, or feedback-headroom");
  const resourceIndex = args.indexOf("--resource-mode");
  const resourceMode = resourceIndex < 0 ? null : args[resourceIndex + 1];
  if (resourceIndex >= 0 && !["per-frame", "persistent"].includes(resourceMode))
    throw new RangeError("--resource-mode must be per-frame or persistent");
  const persistent = resourceMode === "persistent";
  const explicitResourceMode = resourceIndex >= 0;
  const hybridIndex = args.indexOf("--hybrid-mode");
  const hybridMode = hybridIndex < 0 ? null : args[hybridIndex + 1];
  if (hybridIndex >= 0 && ![
    "png-static-roi", "png-quiescent-detail", "hevc-quiescent-tiles",
  ].includes(hybridMode))
    throw new RangeError("--hybrid-mode must be png-static-roi, png-quiescent-detail, or hevc-quiescent-tiles");
  const hybridCacheIndex = args.indexOf("--hybrid-cache-mode");
  const hybridCacheMode = hybridCacheIndex < 0 ? "suppress-reactivate" : args[hybridCacheIndex + 1];
  if (hybridCacheIndex >= 0 && !["dda-gated", "suppress-reactivate"].includes(hybridCacheMode))
    throw new RangeError("--hybrid-cache-mode must be dda-gated or suppress-reactivate");
  if (hybridCacheIndex >= 0 && hybridMode !== "png-quiescent-detail")
    throw new Error("--hybrid-cache-mode requires --hybrid-mode png-quiescent-detail");
  const rateControlIndex = args.indexOf("--rate-control");
  const rateControlMode = rateControlIndex < 0 ? null : args[rateControlIndex + 1];
  if (rateControlIndex >= 0 && rateControlMode !== "external-budget-vbr")
    throw new RangeError("--rate-control must be external-budget-vbr");
  if (Number(dirtyQpMap) + Number(main10) + Number(roiIndex >= 0) +
      Number(explicitResourceMode) + Number(hybridIndex >= 0) + Number(rateControlIndex >= 0) > 1)
    throw new Error("--dirty-qp-map, --roi-mode, --main10, --resource-mode, --hybrid-mode, and --rate-control are separate hypotheses");
  const requestedTopology = parseWssTopologyOptions(args);
  const controlTopology = { laneCount: 16, dataShards: 15, parityShards: 1 };
  if ((explicitResourceMode || roiIndex >= 0 || hybridIndex >= 0 || rateControlIndex >= 0) && requestedTopology &&
      (requestedTopology.laneCount !== 16 || requestedTopology.dataShards !== 15 ||
       requestedTopology.parityShards !== 1))
    throw new Error("pipeline experiments require the unchanged 16-lane 15+1 control topology");
  const common = {
    schemaVersion: 2,
    implementationVariant: "candidate",
    preset: "p3",
    tuning: "ultra-low-latency",
    multipass: "disabled",
    aqMode: "spatial",
    aqStrength: 8,
    vbvFrames: 1,
    lookaheadFrames: 0,
    weightedPrediction: false,
    gopSeconds: 2,
    bitrateMbps: 6,
  };
  if (main10) return {
    id: "hevc-main10",
    ...common,
    experimentId: "hevc-main10",
    dirtyQpMap: false,
    main10: true,
    wssTopology: controlTopology,
  };
  if (dirtyQpMap) return {
    id: "dirty-qp-map",
    ...common,
    experimentId: "dirty-qp-map",
    dirtyQpMap: true,
    main10: false,
    wssTopology: requestedTopology ?? controlTopology,
  };
  if (roiMode) return {
    id: `roi-${roiMode}`,
    ...common,
    experimentId: `roi-${roiMode}`,
    dirtyQpMap: false,
    main10: false,
    roiMode,
    resourceMode: "per-frame",
    asyncDepth: 0,
    hybridMode: "off",
    wssTopology: controlTopology,
  };
  if (hybridMode) return {
    id: `hybrid-${hybridMode}${hybridCacheMode === "dda-gated" ? "-dda-gated" : ""}`,
    ...common,
    experimentId: `hybrid-${hybridMode}${hybridCacheMode === "dda-gated" ? "-dda-gated" : ""}`,
    dirtyQpMap: false,
    main10: false,
    roiMode: "off",
    resourceMode: "per-frame",
    asyncDepth: 0,
    hybridMode,
    hybridCacheMode,
    wssTopology: controlTopology,
  };
  if (rateControlMode) return {
    id: "external-budget-vbr-cq23",
    ...common,
    experimentId: "external-budget-vbr-cq23",
    vbvFrames: 4,
    dirtyQpMap: false,
    main10: false,
    roiMode: "off",
    resourceMode: "per-frame",
    asyncDepth: 0,
    hybridMode: "off",
    rateControlMode,
    targetQuality: 23,
    peakBitrateMultiplier: 4,
    requestedBitrateMbps: 6,
    totalMediaCapMbps: 6,
    wssTopology: controlTopology,
  };
  if (resourceMode === "per-frame") return {
    id: "nvenc-per-frame-control",
    ...common,
    implementationVariant: "production",
    experimentId: "nvenc-per-frame-control",
    dirtyQpMap: false,
    main10: false,
    roiMode: "off",
    resourceMode: "per-frame",
    asyncDepth: 0,
    hybridMode: "off",
    wssTopology: controlTopology,
  };
  if (persistent) return {
    id: "nvenc-persistent",
    ...common,
    experimentId: "nvenc-persistent",
    dirtyQpMap: false,
    main10: false,
    roiMode: "off",
    resourceMode: "persistent",
    asyncDepth: 0,
    hybridMode: "off",
    wssTopology: controlTopology,
  };
  return {
    id: "quality-latency-control",
    ...common,
    implementationVariant: "production",
    experimentId: "quality-latency-control",
    dirtyQpMap: false,
    main10: false,
    roiMode: "off",
    resourceMode: "per-frame",
    asyncDepth: 0,
    hybridMode: "off",
    wssTopology: requestedTopology ?? controlTopology,
  };
}

export function buildManualCandidate(args) {
  const experimentFlags = new Set([
    "--dirty-qp-map", "--roi-mode", "--resource-mode", "--hybrid-mode", "--hybrid-cache-mode", "--main10", "--rate-control",
  ]);
  const effectiveArgs = args.some(value => experimentFlags.has(value))
    ? args
    : [...args, "--hybrid-mode", "png-quiescent-detail"];
  const candidate = buildScreeningCandidate(effectiveArgs);
  if (!candidate || candidate.implementationVariant !== "candidate")
    throw new RangeError("manual use requires a default-off candidate experiment");
  return candidate;
}

function artifactRoot(env = process.env) {
  return resolve(env.BUBBLEPLAY_STREAM_LAB_ROOT || join(projectRoot, "artifacts", "stream-lab"));
}

function newRunId(prefix) {
  void prefix;
  return randomUUID();
}

function newestResumableCampaign(root) {
  if (!existsSync(root)) return null;
  return readdirSync(root).map(name => ({ name, path: join(root, name, "campaign-state.json") }))
    .filter(item => existsSync(item.path)).map(item => ({ ...item, state: JSON.parse(readFileSync(item.path, "utf8")), mtime: statSync(item.path).mtimeMs }))
    .filter(item => item.state.status !== "complete" && item.state.status !== "invalid" &&
      item.state.invalidated?.resumable !== false).sort((a, b) => b.mtime - a.mtime)[0]?.name ?? null;
}

function newestResumableTopology(root) {
  if (!existsSync(root)) return null;
  return readdirSync(root).map(name => ({ name, path: join(root, name, "topology-checkpoint.json") }))
    .filter(item => existsSync(item.path)).map(item => ({ ...item, state: JSON.parse(readFileSync(item.path, "utf8")), mtime: statSync(item.path).mtimeMs }))
    .filter(item => item.state.status !== "complete").sort((a, b) => b.mtime - a.mtime)[0]?.name ?? null;
}

function resumeArgument(args, root, command) {
  const index = args.indexOf("--resume");
  if (index < 0) return null;
  const explicit = args[index + 1];
  if (explicit && !explicit.startsWith("--")) return explicit;
  const latest = command === "topology-sweep" ? newestResumableTopology(root) : newestResumableCampaign(root);
  if (!latest) throw new Error("No incomplete Stream Lab campaign is available to resume.");
  return latest;
}

async function runLabCommand(command, args, env) {
  const root = artifactRoot(env);
  const baseUrl = option(args, "--url", env.BUBBLEPLAY_URL);
  const workerVersionOverride = parseWorkerVersionOptions(args, env);
  const resumeRun = ["campaign", "optimize", "topology-sweep"].includes(command) ? resumeArgument(args, root, command) : null;
  const runId = resumeRun ?? option(args, "--run-id", newRunId(command));
  validateRunId(runId);
  const transientVdd = args.includes("--transient-vdd");
  const { loopbackRelay } = validateLiveRunRouting(baseUrl, workerVersionOverride, transientVdd);
  const store = artifactStore(root, runId);
  const prepared = prepareLabExecution(command, env);
  const preflight = prepared.preflight;
  const manifest = {
    command,
    transport: "wss",
    mode: "work",
    codec: "hevc",
    quality: { width: 1920, height: 1080, fps: 30 },
    scenes: SCENES,
    receiverProfile: preflight.receiverProfile,
    preflight,
    compatibility: {
      expected: STREAM_LAB_COMPATIBILITY,
      workerVersionOverride,
      receiverAssets: "local",
    },
    displayLease: transientVdd ? "lab-helper-transient-vdd" : "broker-persistent-vdd",
    campaign: ["sweep", "campaign", "optimize"].includes(command)
      ? (command === "optimize" ? optimizationCampaignPlan() : encoderCampaignPlan()) : undefined,
  };
  if (!resumeRun) store.initialize(manifest);
  let campaignState = ["campaign", "optimize"].includes(command)
    ? (resumeRun ? store.readCampaignState() : command === "optimize"
      ? createCampaignStateV2({ runId })
      : { schemaVersion: 1, runId, status: "running", units: {}, updatedAt: new Date().toISOString() })
    : null;
  if (resumeRun && (campaignState?.status === "invalid" || campaignState?.invalidated?.resumable === false))
    throw new Error(`Stream Lab run ${resumeRun} is invalidated and cannot be resumed; start a fresh run.`);
  if (campaignState && !resumeRun) store.writeCampaignState(campaignState);
  if (command === "optimize") {
    const validation = validateCampaignStateV2(campaignState);
    if (!validation.valid) throw new Error(`Invalid optimization checkpoint: ${validation.reasons.join(", ")}`);
  }
  if (preflight.missing.length) {
    const metrics = { schemaVersion: 1, result: "unavailable", missing: preflight.missing, message: "Stream Lab preflight did not pass." };
    store.writeMetrics(metrics);
    return { exitCode: 2, runId, artifactPath: store.path, ...metrics };
  }
  try {
    let candidateHost;
    try {
      candidateHost = selectCandidateHostForRun(args, env, prepared);
    } catch (error) {
      if (error?.code === "candidate-host-compatibility-mismatch" && error?.detail)
        store.updateManifest({ candidateHost: error.detail });
      throw error;
    }
    store.updateManifest({ candidateHost: inspectCandidateHostCompatibility(candidateHost) });
    const { runLiveBaseline, runLiveLaneRecovery, runLiveManual, runLiveOptimize, runLiveSweep, runLiveTopologySweep } =
      await import("./stream-lab-live.mjs");
    const common = {
      runId, store, env,
      candidateHost,
      baseUrl,
      authTimeoutMs: Number(option(args, "--auth-timeout-ms", 120_000)),
      workerVersionOverride,
      receiverAssetOverride: receiverAssetInterceptionForRun(loopbackRelay),
      transientVdd,
      ...(parseWssTopologyOptions(args) ? { wssTopology: parseWssTopologyOptions(args) } : {}),
      ...(command === "topology-sweep" && resumeRun ? { resumeCheckpoint: store.readTopologyCheckpoint() } : {}),
    };
    const bandwidthOption = option(args, "--bandwidth", null);
    let metrics;
    if (command === "manual") {
      const candidate = buildManualCandidate(args);
      metrics = await runLiveManual({
        ...common,
        candidate,
        wssTopology: candidate.wssTopology,
        onReady: report => process.stdout.write(`${JSON.stringify(report, null, 2)}\n`),
      });
    } else if (["baseline", "smoke"].includes(command)) {
      const sceneIds = command === "smoke" ? ["ui-text", "localized-change"] : parseSceneOptions(args);
      const dirtyQpMap = args.includes("--dirty-qp-map");
      const roiMode = option(args, "--roi-mode", null);
      const roiCandidate = dirtyQpMap || roiMode !== null;
      const hybridCandidate = option(args, "--hybrid-mode", null) !== null;
      const candidate = buildScreeningCandidate(args);
      metrics = await runLiveBaseline({ ...common,
          ...(candidate ? { candidate } : {}),
          bandwidthMbps: bandwidthOption === null ? null : Number(bandwidthOption),
          performanceSeconds: Number(option(args, "--seconds", command === "smoke" ? 5 : 15)),
          repetitions: Number(option(args, "--repetitions", command === "smoke" ? 1 : 3)),
          qualitySamples: Number(option(args, "--quality-samples", 12)),
          deltaQuality: args.includes("--delta-quality"),
          deltaTriggerFrames: parseDeltaTriggerFrames(args),
          performanceScene: option(args, "--performance-scene",
            roiCandidate || hybridCandidate ? "localized-change" : "motion-60"),
          actionProbe: performanceActionProbeEnabled(args, roiCandidate, hybridCandidate),
          ...(sceneIds ? { sceneIds } : {}),
        });
    } else if (command === "topology-sweep") {
      const requestedTopology = parseWssTopologyOptions(args);
      metrics = await runLiveTopologySweep({ ...common,
        ...(requestedTopology ? { topologies: [requestedTopology] } : {}),
        bandwidthMbps: Number(option(args, "--bandwidth", 4)),
        performanceSeconds: Number(option(args, "--seconds", 10)),
        quality: args.includes("--quality"),
        repetitions: Number(option(args, "--repetitions", args.includes("--quality") ? 3 : 1)),
        qualitySamples: Number(option(args, "--quality-samples", 8)),
      });
    } else if (command === "lane-recovery") {
      const sceneIds = parseSceneOptions(args);
      metrics = await runLiveLaneRecovery({ ...common,
        actionProbe: performanceActionProbeEnabled(args, false, false),
        bandwidthMbps: Number(option(args, "--bandwidth", 4)),
        performanceSeconds: Number(option(args, "--seconds", 8)),
        qualitySamples: Number(option(args, "--quality-samples", 8)),
        qualityRepetitions: Number(option(args, "--quality-repetitions", 2)),
        ...(sceneIds ? { sceneIds } : {}),
        stripe: Number(option(args, "--lane", 0)),
        lossDurationMs: Number(option(args, "--loss-ms", 1_500)),
        maxRecoveryMs: Number(option(args, "--recovery-timeout-ms", 5_000)),
      });
    } else if (command === "optimize") {
      const activityGuard = await launchPhysicalActivityGuard({ idleMs: 600_000 });
      try {
        metrics = await runOptimizationLoop({
          state: campaignState,
          activityPolicy: "tag",
          activitySnapshot: () => activityGuard.snapshot(),
          assertRunnable: () => activityGuard.assertRunnable(),
          waitUntilIdle: onProgress => activityGuard.waitUntilIdle(onProgress),
          cleanup: async () => {},
          writeState(nextState) { campaignState = nextState; store.writeCampaignState(nextState); },
          runEpoch: async ({ state, assertRunnable, activitySnapshot, writeCheckpoint }) => {
            let epochState = state;
            const onCheckpoint = async (unitId, result) => {
              const stage = unitId.startsWith("opt:baseline:") ? 0 : 2;
              epochState = { ...epochState, status: "running", units: { ...epochState.units, [unitId]: result },
                checkpoint: { ...epochState.checkpoint, stage, unitId, phase: unitId.split(":").slice(0, 2).join("-") },
                updatedAt: new Date().toISOString() };
              writeCheckpoint(epochState);
            };
            return runLiveOptimize({ ...common, resumeState: state, onCheckpoint,
              assertCampaignActive: assertRunnable,
              activitySnapshot,
              optimizationPerformanceSeconds: Number(option(args, "--seconds", 5)),
              optimizationQualitySamples: Number(option(args, "--quality-samples", 12)),
              abbaRepetitions: Number(option(args, "--abba-repetitions", 1)),
              gates: state.gates });
          },
        });
      } finally { activityGuard.stop(); }
    } else {
      metrics = await runLiveSweep({ ...common,
          screeningSeconds: Number(option(args, "--seconds", 15)),
          soakSeconds: Number(option(args, "--soak-seconds", 600)),
          adaptiveDwellSeconds: Number(option(args, "--adaptive-dwell-seconds", 120)),
          regressionSeconds: Number(option(args, "--regression-seconds", 15)),
          compatibilitySeconds: Number(option(args, "--compatibility-seconds", 5)),
          idleSeconds: Number(option(args, "--idle-seconds", 60)),
          resumeState: campaignState,
          onCheckpoint: campaignState ? async (unitId, result) => {
            campaignState = { ...campaignState, status: "running", units: { ...campaignState.units, [unitId]: result }, updatedAt: new Date().toISOString() };
            store.writeCampaignState(campaignState);
          } : null,
        });
    }
    if (["sweep", "campaign"].includes(command)) {
      const acceptance = evaluateCampaignAcceptance(metrics);
      metrics = { ...metrics, accepted: acceptance.accepted, acceptance,
        result: acceptance.accepted ? "complete" : "rejected" };
    }
    store.writeMetrics(metrics);
    if (campaignState) {
      campaignState = command === "optimize"
        ? { ...campaignState, status: "running", frontier: metrics.frontier,
            gates: metrics.ratchetedGates ?? campaignState.gates,
            checkpoint: { stage: 3, epoch: campaignState.checkpoint.epoch, unitId: null, phase: "awaiting-code-hypothesis" },
            updatedAt: new Date().toISOString() }
        : { ...campaignState, status: "complete", accepted: metrics.accepted,
            completedAt: new Date().toISOString(), updatedAt: new Date().toISOString() };
      store.writeCampaignState(campaignState);
    }
    return { exitCode: reportExitCode(metrics), runId, artifactPath: store.path, ...metrics };
  } catch (error) {
    const unavailable = error?.name === "StreamLabUnavailable" || error?.name === "ServiceControlError";
    const metrics = {
      schemaVersion: 1, result: unavailable ? "unavailable" : "fail",
      error: { code: error?.code || "unexpected-error", message: String(error?.message || error), detail: error?.detail ?? null },
    };
    store.writeMetrics(metrics);
    store.writeFailure(metrics.error);
    if (campaignState) store.writeCampaignState(command === "optimize"
      ? failedOptimizationState(campaignState, metrics.error)
      : { ...campaignState, status: "interrupted", error: metrics.error, updatedAt: new Date().toISOString() });
    return { exitCode: unavailable ? 2 : 1, runId, artifactPath: store.path, ...metrics };
  }
}

export async function runProductionProbeCommand(args, env = process.env) {
  const root = artifactRoot(env);
  const runId = option(args, "--run-id", newRunId("production-probe"));
  validateRunId(runId);
  const store = artifactStore(root, runId);
  const durationMs = Math.max(1_000, Number(option(args, "--seconds", 180)) * 1_000);
  const intervalMs = Math.max(100, Number(option(args, "--interval-ms", 250)));
  const baseUrl = option(args, "--url", env.BUBBLEPLAY_URL);
  const wssTopology = parseWssTopologyOptions(args);
  store.initialize({
    command: "production-probe",
    transport: "wss",
    installedHost: true,
    candidateHostStarted: false,
    inputAutomation: false,
    receiverProfile: persistentReceiverProfile(env),
    baseUrl: baseUrl || "https://playfull-cat.your-subdomain.workers.dev/",
    durationMs,
    intervalMs,
    ...(wssTopology ? { wssTopology } : {}),
  });
  try {
    const { runProductionProbe } = await import("./stream-lab-live.mjs");
    const report = await runProductionProbe({
      env,
      store,
      baseUrl,
      durationMs,
      intervalMs,
      runId,
      ...(wssTopology ? { wssTopology } : {}),
      authTimeoutMs: Number(option(args, "--auth-timeout-ms", 30_000)),
      port: Number(option(args, "--port", 19343)),
    });
    const { samples: _samples, ...metrics } = report;
    store.writeMetrics(metrics);
    return { exitCode: reportExitCode(metrics), runId, artifactPath: store.path, ...metrics };
  } catch (error) {
    const unavailable = error?.name === "StreamLabUnavailable";
    const metrics = {
      schemaVersion: 1,
      result: unavailable ? "unavailable" : "fail",
      error: {
        code: error?.code || "unexpected-error",
        message: String(error?.message || error),
        detail: error?.detail ?? null,
      },
    };
    store.writeMetrics(metrics);
    store.writeFailure(metrics.error);
    return { exitCode: unavailable ? 2 : 1, runId, artifactPath: store.path, ...metrics };
  }
}

export function reportExitCode(report) {
  if (report?.result === "unavailable") return 2;
  return report?.result === "rejected" || report?.accepted === false || report?.result === "fail" ? 1 : 0;
}

export function failedOptimizationState(state, error, updatedAt = new Date().toISOString()) {
  return {
    ...state,
    status: "failed",
    pauseState: { paused: false, reason: null, since: null, resumeAfter: null },
    error,
    updatedAt,
  };
}

export function cleanLocalExpiredRuns(env = process.env, now = Date.now()) {
  const runsRoot = join(dirname(persistentReceiverProfile(env)), "runs");
  if (!existsSync(runsRoot)) return { removed: [], runsRoot };
  const removed = [];
  for (const name of readdirSync(runsRoot)) {
    if (!/^[A-Za-z0-9][A-Za-z0-9._-]{0,79}$/.test(name)) continue;
    const path = join(runsRoot, name);
    const leasePath = join(path, "lease.json");
    if (!existsSync(leasePath)) continue;
    try {
      const lease = JSON.parse(readFileSync(leasePath, "utf8"));
      if (lease.runId !== name || Date.parse(lease.expiresAt) > now) continue;
      rmSync(path, { recursive: true, force: true });
      removed.push(name);
    } catch { /* An invalid lease is not authority to delete anything. */ }
  }
  return { removed, runsRoot };
}

async function main(argv = process.argv.slice(2), env = process.env) {
  const [command = "help", ...args] = argv;
  if (command === "preflight") {
    const report = runPreflight({ env });
    process.stdout.write(`${JSON.stringify(report, null, 2)}\n`);
    if (report.result !== "ready") process.exitCode = 2;
    return;
  }
  if (["manual", "baseline", "smoke", "sweep", "campaign", "optimize", "topology-sweep", "lane-recovery"].includes(command)) {
    const report = await runLabCommand(command, args, env);
    process.stdout.write(`${JSON.stringify(report, null, 2)}\n`);
    process.exitCode = report.exitCode;
    return;
  }
  if (command === "production-probe") {
    const report = await runProductionProbeCommand(args, env);
    process.stdout.write(`${JSON.stringify(report, null, 2)}\n`);
    process.exitCode = report.exitCode;
    return;
  }
  if (command === "compare") {
    const positionals = args.filter((value, index) => value !== "--" && args[index - 1] !== "--output" && value !== "--output");
    if (positionals.length < 2) throw new Error("usage: stream-lab compare <baseline metrics|run> <candidate metrics|run> [--output path]");
    const comparison = compareCandidateFiles(loadMetrics(positionals[0]), loadMetrics(positionals[1]));
    const output = option(args, "--output");
    if (output) {
      mkdirSync(dirname(resolve(output)), { recursive: true });
      writeFileSync(resolve(output), `${JSON.stringify(comparison, null, 2)}\n`);
    }
    process.stdout.write(`${JSON.stringify(comparison)}\n`);
    if (!comparison.accepted) process.exitCode = 1;
    return;
  }
  if (command === "cleanup") {
    const report = cleanLocalExpiredRuns(env);
    process.stdout.write(`${JSON.stringify({ result: "pass", ...report }, null, 2)}\n`);
    return;
  }
  process.stdout.write("BubblePlay Stream Lab\n\nRemote live runs require --worker-version-id UUID for a version admitted to the active deployment and accept --worker-name NAME (default playfull-cat). Pinned remote runs may add --transient-vdd. Use --candidate-host with an existing uninstalled binary when the default target is occupied. Explicit HTTP loopback runs use the local compatibility descriptor, omit Worker version flags, and may also add --transient-vdd.\n\nCommands: preflight, production-probe [--seconds N --interval-ms N --wss-lanes N --wss-parity 0|1|2], manual [candidate flags; defaults to png-quiescent-detail], smoke, baseline [--no-action-probe --dirty-qp-map|--roi-mode damage|text-edge|feedback|feedback-headroom|--hybrid-mode png-static-roi|png-quiescent-detail|hevc-quiescent-tiles [--hybrid-cache-mode dda-gated|suppress-reactivate]|--rate-control external-budget-vbr|--main10|--resource-mode per-frame|persistent --performance-scene id --quality-samples N --delta-quality --delta-trigger-frames N], sweep, topology-sweep [--quality --repetitions N --quality-samples N] [--wss-lanes N --wss-parity 0|1|2], lane-recovery [--scenes id,id --quality-repetitions N --lane N --loss-ms N --recovery-timeout-ms N], campaign, optimize [--resume [run-id]], compare, cleanup\n");
}

const isMain = process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url);
if (isMain) await main();
