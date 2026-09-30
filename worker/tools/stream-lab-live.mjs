import { finiteOrNull, frameCount } from "./stream-lab-measurement.mjs";
import { decoderSupportsCodec } from "../public/transport.js";
import { chooseOptimizationFinalists, summarizeAlignmentFailure, evaluateWssCohortContinuity, evaluateWssCohortProgress, advanceWssCohortAdmission, hybridSceneRequirement, shouldRunQuiescentInteractionTrials, qualityAnchorAlignmentOptions, quiescentInteractionReady, quiescentTransitionCommitted, quiescentCursorSweepSettled, quiescentCursorCounterChanges, exactFrameAlignment, performanceFrameTrace, buildTemporalCapturePlan, HYBRID_SCENES } from "./stream-lab-measurement.mjs";
export { advanceWssCohortAdmission, buildTemporalCapturePlan, chooseOptimizationFinalists, evaluateWssCohortContinuity, evaluateWssCohortProgress, exactFrameAlignment, hybridSceneRequirement, performanceFrameTrace, qualityAnchorAlignmentOptions, quiescentCursorCounterChanges, quiescentCursorSweepSettled, quiescentInteractionReady, quiescentTransitionCommitted, shouldRunQuiescentInteractionTrials, summarizeAlignmentFailure } from "./stream-lab-measurement.mjs";
import { runCheckpointedUnit, runFreshCheckpoint, SerializedLeaseHeartbeat, installTerminationCleanup, interruptedCampaignState, StreamLabUnavailable } from "./stream-lab-lifecycle.mjs";
export { runCheckpointedUnit, runFreshCheckpoint, SerializedLeaseHeartbeat, installTerminationCleanup, interruptedCampaignState, StreamLabUnavailable } from "./stream-lab-lifecycle.mjs";
import { closeSync, existsSync, mkdirSync, openSync, readFileSync, readdirSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { spawn, spawnSync } from "node:child_process";
import { randomUUID } from "node:crypto";
import {
  STREAM_LAB_ASSET_BUILD,
  STREAM_LAB_COMPATIBILITY,
} from "../public/media.js";

import {
  SCENES,
  characterErrorRate,
  compareCandidates,
  median,
  medianAbsoluteDeviation,
  percentile,
  rankCandidates,
  readImageLuma,
  runFfmpegQuality,
  runTesseractOcr,
  scoreLumaTiles,
  temporalUnchangedRegionError,
} from "./stream-lab-core.mjs";
import {
  aggregateParityVector,
  buildAbbaOrder,
  evaluateParetoAcceptance,
  generateArtifactRefinementConfigurations,
  generatePairwiseEncoderConfigurations,
  ratchetGates,
  selectParetoFrontier,
  summarizeSourceParityLoss,
} from "./stream-lab-parity.mjs";
import {
  SERVICE_STATE,
  buildEncoderRound,
  findChrome,
  inspectLabService,
  openDemandStartedHelperSession,
  persistentReceiverProfile,
} from "./stream-lab-runtime.mjs";

const sleep = ms => new Promise(resolveSleep => setTimeout(resolveSleep, ms));
const DEFAULT_URL = "https://playfull-cat.your-subdomain.workers.dev/";
const CAMPAIGN_BASELINES = Object.freeze([
  { bandwidthMbps: null, repetitions: 1, label: "unshaped" }, { bandwidthMbps: 2, repetitions: 1, label: "2m" },
  { bandwidthMbps: 4, repetitions: 3, label: "4m" }, { bandwidthMbps: 8, repetitions: 1, label: "8m" },
  { bandwidthMbps: 12, repetitions: 1, label: "12m" },
]);
const CAMPAIGN_STABILITY_IDS = Object.freeze([
  "stability:decoder-fault-recovery", "stability:sharp-viewport-2x", "stability:adaptive-bandwidth:8m-1",
  "stability:adaptive-bandwidth:4m-1", "stability:adaptive-bandwidth:2m", "stability:adaptive-bandwidth:4m-2",
  "stability:adaptive-bandwidth:8m-2", "stability:adaptive-bandwidth-staircase", "stability:wss-gaming",
  "stability:webrtc-gaming-unshaped", "stability:webrtc-gaming-relay-8m", "stability:h264-fallback-smoke",
  "stability:post-cleanup-idle",
]);
const ALLOWLISTED_SOURCE_FILES = Object.freeze(["worker/public/index.html", "host/src/main.rs"]);

export function campaignCheckpointContract() {
  return {
    baseline: CAMPAIGN_BASELINES.flatMap(entry => Array.from({ length: entry.repetitions }, (_, index) => `baseline:${entry.label}:r${index + 1}`)),
    stability: [...CAMPAIGN_STABILITY_IDS],
  };
}

export function aggressiveOptimizationContract() {
  const candidates = generatePairwiseEncoderConfigurations({ mode: "work", count: 96 });
  const refinementCandidates = generateArtifactRefinementConfigurations();
  const baseline = [null, 2, 4, 8, 12].flatMap(bandwidth =>
    [1, 2, 3].map(repetition => `opt:baseline:${bandwidth ?? "unshaped"}:r${repetition}`));
  const screen = candidates.flatMap(candidate => [null, 4].map(bandwidth =>
    `opt:screen:${candidate.experimentId}:${bandwidth ?? "unshaped"}`));
  const refinementBaseline = [null, 4].flatMap(bandwidth => [1, 2, 3].map(repetition =>
    `opt:refine-baseline:${bandwidth ?? "unshaped"}:r${repetition}`));
  const refinement = refinementCandidates.flatMap(candidate => [null, 4].map(bandwidth =>
    `opt:refine:${candidate.experimentId}:${bandwidth ?? "unshaped"}`));
  return {
    baseline,
    screen,
    refinementBaseline,
    refinement,
    validationBandwidths: [2, 4, 8],
    abbaBandwidths: [2, 4, 8, 12],
    abbaOrder: buildAbbaOrder("production", "candidate", 1).map(unit => unit.variant),
  };
}

/** A native frame is only decode evidence while the newest RTC sample still
 * describes the current peer epoch. A HUD label plus counters left over from a
 * previous peer must never satisfy HEVC acceptance.
 *
 * Chromium RTC timestamps and firstFrameAtMs use wall-clock milliseconds;
 * video callback times use performance time and cannot be compared to them.
 * Check nullable values directly because Number(null) would become zero. */
export const NATIVE_RENDERED_AGE_LIMIT_MS = 1_500;

export function nativeFrameState(snapshot) {
  const native = snapshot?.nativePresentation ?? null;
  const inbound = snapshot?.rtc?.inbound ?? null;
  const width = native?.width;
  const height = native?.height;
  const framesPresented = native?.framesPresented;
  const firstFrameAtMs = native?.firstFrameAtMs;
  const sampleAt = inbound?.timestamp;
  const framesDecoded = inbound?.framesDecoded;
  const renderedAgeMs = snapshot?.renderedAgeMs;
  const state = {
    supported: native?.supported === true,
    presented: Number.isFinite(framesPresented) && framesPresented > 0,
    sized: Number.isFinite(width) && width > 0 && Number.isFinite(height) && height > 0,
    firstFrameRecorded: Number.isFinite(firstFrameAtMs),
    codecOkay: /hevc|h265|hev1/i.test(String(snapshot?.rtc?.codec?.mimeType || "")),
    // The stats sample must come from the same peer generation and must be at
    // or after the first presented frame of that generation.
    epochMatches: Number.isInteger(native?.generation) &&
      native.generation === snapshot?.rtc?.generation,
    statsAfterFirstFrame: Number.isFinite(sampleAt) && Number.isFinite(firstFrameAtMs) &&
      sampleAt >= firstFrameAtMs,
    decoded: Number.isFinite(framesDecoded) && framesDecoded > 0,
    freshFrame: Number.isFinite(renderedAgeMs) && renderedAgeMs >= 0 &&
      renderedAgeMs <= NATIVE_RENDERED_AGE_LIMIT_MS,
  };
  state.ready = state.supported && state.presented && state.sized && state.firstFrameRecorded &&
    state.codecOkay && state.epochMatches && state.statsAfterFirstFrame && state.decoded &&
    state.freshFrame;
  return state;
}

export function firstFrameReadiness(snapshot, options) {
  const { expected, preApplyBenchStatus, runId, transport, relay, expectedTimestamp } = options;
  const decoderPath = normalizeDecoderPath(options.decoderPath);
  const nativePresented = Number(snapshot?.nativePresentation?.framesPresented) > 0;
  const rendered = Number.isFinite(snapshot?.renderedTimestamp) || nativePresented;
  const routeOkay = transport === "wss" ? snapshot?.route === "wss" : relay === true ?
    isRelayRoute(snapshot?.route) : relay === false ? isPeerToPeerRoute(snapshot?.route) : isRtcRoute(snapshot?.route);
  // The HUD `dec` label is not decode evidence. Native playback must prove the
  // <video> element presented HEVC frames for the current peer epoch.
  const decoderOkay = expected.codec === "h264" ? /h264|avc1/i.test(snapshot?.dec ?? "")
    : transport === "webrtc" && decoderPath === "native" ? nativeFrameState(snapshot).ready
    : /hevc|h265|hev1/i.test(snapshot?.dec ?? "");
  const benchOkay = benchStatusConfirmsApply(preApplyBenchStatus, snapshot?.benchStatus, expected, runId);
  const fresh = Number.isFinite(expectedTimestamp) && timestampReached(snapshot?.glassToGlass?.timestamp, expectedTimestamp >>> 0);
  return { rendered, routeOkay, decoderOkay, benchOkay, fresh,
    ready: rendered && routeOkay && decoderOkay && benchOkay && fresh };
}

/** Persistent monitor selection only counts as a host rejection when the host
 * actually answered with a monitor error for the exact target. A receiver that
 * could not issue the control is a deferral, not a rejection. */
export function classifyMonitorSelectionFailure(evidence = {}) {
  if (!evidence.sawRegistered) return { code: "signaling-registration-timeout",
    message: "Signaling did not register before persistent monitor selection timed out." };
  if (!evidence.sawFreshMonitorList) return { code: "monitor-list-timeout",
    message: "The host did not publish a fresh monitor list after signaling registration." };
  if (!evidence.sawExactTarget) return { code: "monitor-ambiguous",
    message: "A fresh host monitor list did not contain exactly one persistent virtual display selector." };
  if (evidence.sawHostRejection) return { code: "monitor-selection-rejected",
    message: "The host rejected the exact persistent virtual display selector." };
  if (evidence.sawDeferredSelection) return { code: "monitor-selection-deferred",
    message: "The receiver never issued the monitor control before persistent monitor selection timed out." };
  return { code: "monitor-switch-timeout",
    message: "The host did not confirm the exact persistent virtual display in a fresh monitor response." };
}

export function hevcTileDecoderAdmission(snapshot, expected = {}) {
  if (expected.hybridMode !== "hevc-quiescent-tiles") {
    return { required: false, ready: true, terminal: false, state: null, codec: null };
  }
  const hybrid = snapshot?.wssMedia?.hybrid;
  const state = typeof hybrid?.decoderCapability === "string"
    ? hybrid.decoderCapability
    : "unprobed";
  const codec = typeof hybrid?.decoderCapabilityCodec === "string"
    ? hybrid.decoderCapabilityCodec
    : null;
  return {
    required: true,
    ready: state === "supported",
    terminal: ["unsupported", "api-unavailable", "error"].includes(state),
    state,
    codec,
  };
}

export function shouldRetryMeasuredFrame(state, elapsedMs, retryCount) {
  return retryCount === 0 && elapsedMs >= 1_000 && state.rendered && state.routeOkay &&
    state.decoderOkay && state.benchOkay && !state.fresh;
}

export function classifySilentDdaFailure(
  state, diagnostics, expectedSource, currentSource, elapsedMs,
) {
  const media = diagnostics?.wssMedia;
  const bench = diagnostics?.benchStatus;
  const sourceFrameDelta = Number(currentSource?.frameId) - Number(expectedSource?.frameId);
  const stalled = elapsedMs >= 1_250 &&
    state?.rendered === true && state?.routeOkay === true && state?.decoderOkay === true &&
    state?.benchOkay === true && state?.fresh === false &&
    currentSource?.paused === false && Number.isFinite(sourceFrameDelta) && sourceFrameDelta > 0 &&
    Number(diagnostics?.renderedAgeMs) >= 1_000 &&
    Number(media?.receivedPackets) > 0 && Number(media?.presented) > 0 &&
    media?.v4?.socketOpen === true &&
    Number(media?.packetRejects?.total) === 0 &&
    Number(bench?.encodeCount) === 0 && Number(bench?.encodedBytes) === 0 &&
    Number(bench?.wssPayloadBytes) === 0;
  if (!stalled) return null;
  return {
    code: "dda-session-stalled",
    sourceFrameDelta,
    renderedAgeMs: Number(diagnostics.renderedAgeMs),
    mediaSocketOpen: true,
    receivedPackets: Number(media.receivedPackets),
  };
}

export function shouldRetryQualityFrameMatch(snapshot, elapsedMs, retryCount) {
  const media = snapshot?.wssMedia;
  return retryCount === 0 && elapsedMs >= 750 &&
    snapshot?.route === "wss" && snapshot?.signaling?.registered === true &&
    Number(media?.receivedBytes) > 0 && media?.recoveryState === "healthy" &&
    media?.v4?.socketOpen === true;
}

export function candidateStartupRejection(error, configuration, bandwidthMbps) {
  if (!["media-prime-timeout", "first-frame-timeout"].includes(error?.code) ||
      configuration?.implementationVariant !== "candidate") return null;
  const diagnostics = error?.detail?.diagnostics ?? error?.detail;
  const bench = diagnostics?.benchStatus;
  const renderedAgeValue = diagnostics?.renderedAgeMs;
  const renderedAgeMs = Number(renderedAgeValue);
  const renderedIsAbsentOrStale = renderedAgeValue === null || renderedAgeValue === undefined ||
    !Number.isFinite(renderedAgeMs) || renderedAgeMs >= 1_000;
  if (diagnostics?.route !== "wss" || diagnostics?.signaling?.registered !== true ||
      bench?.implementationVariant !== "candidate" || bench?.experimentId !== configuration.experimentId ||
      Number(bench?.encodedBytes) !== 0 || Number(bench?.wssPayloadBytes) !== 0 || Number(bench?.encodeCount) !== 0 ||
      !renderedIsAbsentOrStale) return null;
  return {
    result: "rejected", experimentId: configuration.experimentId, implementationVariant: "candidate",
    configuration, bandwidthMbps,
    rejection: { code: "encoder-candidate-startup-failed", sourceCode: error.code,
      message: String(error.message || error), encodedBytes: 0, offeredBytes: 0, encodeCount: 0 },
  };
}

const CANDIDATE_MEDIA_GATE_REASONS = new Set([
  "decoder-reset", "dependency-loss", "freeze", "stream-latency", "queue-growth", "offered-cap-overshoot",
]);

const BANDWIDTH_SHAPING_REASONS = new Set([
  "qos-policy-unverified", "delivered-cap-overshoot", "offered-cap-overshoot",
]);

export function candidateRuntimeRejection(error, configuration, bandwidthMbps) {
  if (configuration?.implementationVariant !== "candidate") return null;
  if (error?.code === "frame-match-timeout") {
    const diagnostics = error?.detail?.diagnostics;
    const bench = diagnostics?.benchStatus;
    const ownedCandidateStream = diagnostics?.route === "wss" && diagnostics?.signaling?.registered === true &&
      bench?.implementationVariant === "candidate" && bench?.experimentId === configuration.experimentId &&
      Number(diagnostics?.wssMedia?.receivedBytes) > 0;
    if (!ownedCandidateStream) return null;
    return {
      result: "rejected", experimentId: configuration.experimentId, implementationVariant: "candidate",
      configuration, bandwidthMbps,
      rejection: { code: "encoder-candidate-runtime-failed", sourceCode: error.code,
        message: String(error.message || error), reasons: ["frame-match-timeout"],
        renderedAgeMs: Number(diagnostics.renderedAgeMs ?? 0),
        receivedBytes: Number(diagnostics.wssMedia.receivedBytes) },
    };
  }
  if (!["bandwidth-shaping-unverified", "performance-hard-gate-failed"].includes(error?.code) ||
      !Number.isFinite(bandwidthMbps)) return null;
  const detail = error?.detail;
  const reasons = Array.isArray(detail?.reasons) ? [...new Set(detail.reasons)] : [];
  const qosVerified = detail?.qosPolicy?.active === true &&
    detail.qosPolicy.bandwidthMbps === bandwidthMbps && detail?.metrics?.bandwidthVerified === true;
  if (!qosVerified || reasons.length === 0 || reasons.some(reason => !CANDIDATE_MEDIA_GATE_REASONS.has(reason)))
    return null;
  return {
    result: "rejected", experimentId: configuration.experimentId, implementationVariant: "candidate",
    configuration, bandwidthMbps,
    rejection: { code: "encoder-candidate-runtime-failed", sourceCode: error.code,
      message: String(error.message || error), reasons,
      decoderResets: Number(detail.metrics.decoderResets ?? 0),
      dependencyLosses: Number(detail.metrics.dependencyLosses ?? 0),
      freezes: Number(detail.metrics.freezes ?? 0),
      latencyP95Ms: Number(detail.metrics.latencyP95Ms ?? 0) },
  };
}

export function shouldRecoverRejectedCandidate(run, wasCompleted) {
  return run?.result === "rejected" && !wasCompleted;
}

export function buildHybridWorkloadPlan(options = {}) {
  const repoRoot = resolve(options.repoRoot ?? resolve(import.meta.dirname, "..", ".."));
  const trackedFiles = options.trackedFiles ?? trackedBubblePlayFiles(repoRoot);
  const allowlistedFiles = ALLOWLISTED_SOURCE_FILES.filter(path => trackedFiles.includes(path));
  const vscodePath = options.vscodePath === undefined ? findVsCode(options.env ?? process.env) : options.vscodePath;
  return [
    { id: "vscode-source", kind: "real-vscode", executable: vscodePath, isolatedProfile: true,
      extensionsDisabled: true, allowlistedFiles, privateContent: false, coverageEligible: false,
      availability: vscodePath && allowlistedFiles.length ? "unsupported" : "unavailable",
      reason: vscodePath && allowlistedFiles.length ?
        "Exact marker injection and verified virtual-display placement are not implemented; parity must fail closed." :
        "VS Code or an allowlisted tracked source file is unavailable." },
    { id: "source-code-fixture", kind: "local-chrome", scene: "source-code", allowlistedFiles,
      deterministic: true, exactAlignment: true, privateContent: false, coverageEligible: true },
    { id: "chrome-text", kind: "local-chrome", scene: "chrome-text", privateContent: false },
    { id: "chrome-canvas", kind: "local-chrome", scene: "chrome-canvas", privateContent: false },
    { id: "generated-video", kind: "generated-video", scene: "generated-video", cached: false, privateContent: false },
  ];
}

export function retainWorstSamplePairs(samples, options = {}) {
  const limit = options.limit ?? 10;
  if (!Number.isSafeInteger(limit) || limit < 0) throw new RangeError("limit must be a non-negative integer");
  const keep = new Set([...samples].sort((a, b) => qualityLossScore(b) - qualityLossScore(a) ||
    String(a.scene).localeCompare(String(b.scene)) || Number(a.frameId) - Number(b.frameId)).slice(0, limit));
  return samples.map(sample => {
    if (keep.has(sample)) return { ...sample, retained: true };
    const artifactPaths = [...new Set([sample.referencePath, sample.decodedPath, ...(sample.artifactPaths ?? [])].filter(Boolean))];
    for (const path of artifactPaths) rmSync(path, { force: true });
    return { ...sample, retained: false, referencePath: null, decodedPath: null, artifactPaths: [] };
  });
}



function qualityLossScore(sample) {
  const losses = [
    Number.isFinite(sample.vmaf) ? (100 - sample.vmaf) / 100 : null,
    Number.isFinite(sample.ssim) ? 1 - sample.ssim : null,
    Number.isFinite(sample.tileSsim?.worst) ? 1 - sample.tileSsim.worst : null,
    Number.isFinite(sample.ocrCer) ? sample.ocrCer : null,
  ].filter(Number.isFinite);
  return losses.length ? Math.max(...losses) : Number.POSITIVE_INFINITY;
}

function trackedBubblePlayFiles(repoRoot) {
  const result = spawnSync("git", ["-C", repoRoot, "ls-files", "--", ...ALLOWLISTED_SOURCE_FILES],
    { encoding: "utf8", windowsHide: true, timeout: 5_000 });
  return result.status === 0 ? String(result.stdout).split(/\r?\n/).filter(Boolean).map(path => path.replaceAll("\\", "/")) : [];
}

function findVsCode(env = process.env) {
  const candidates = [
    env.LOCALAPPDATA && join(env.LOCALAPPDATA, "Programs", "Microsoft VS Code", "Code.exe"),
    env.ProgramFiles && join(env.ProgramFiles, "Microsoft VS Code", "Code.exe"),
  ].filter(Boolean);
  return candidates.find(existsSync) ?? null;
}

export function selectTransientDisplay(before, after) {
  const existing = new Set(before.map(screen => screen.deviceName.toLowerCase()));
  const added = after.filter(screen => !existing.has(screen.deviceName.toLowerCase()));
  if (added.length === 0) throw new StreamLabUnavailable("vdd-not-created", "The helper did not create a new Windows display.");
  if (added.length !== 1) throw new StreamLabUnavailable("vdd-ambiguous", `Expected one transient display; found ${added.length}.`, added);
  const display = added[0];
  if (display.width !== 1920 || display.height !== 1080)
    throw new StreamLabUnavailable("vdd-geometry", `Transient display must be 1920×1080, got ${display.width}×${display.height}.`, display);
  return display;
}

export function windowCoversDisplay(bounds, display, tolerance = 32) {
  if (![bounds?.left, bounds?.top, bounds?.width, bounds?.height, display?.x, display?.y, display?.width, display?.height].every(Number.isFinite)) return false;
  return bounds.left <= display.x + tolerance && bounds.top <= display.y + tolerance &&
    bounds.left + bounds.width >= display.x + display.width - tolerance &&
    bounds.top + bounds.height >= display.y + display.height - tolerance;
}

export async function placeSourceWindow(page, processId, display, options = {}) {
  const nativePlace = options.nativePlace ?? placeNativeWindow;
  const wait = options.sleep ?? sleep;
  let sourceWindow = await page.call("Browser.getWindowForTarget");
  for (let attempt = 0; attempt < 2; attempt++) {
    await page.call("Browser.setWindowBounds", { windowId: sourceWindow.windowId, bounds: { windowState: "normal" } });
    await page.call("Browser.setWindowBounds", { windowId: sourceWindow.windowId,
      bounds: { left: display.x, top: display.y, width: display.width, height: display.height } });
    nativePlace(processId, display);
    await wait(100);
    await page.call("Browser.setWindowBounds", { windowId: sourceWindow.windowId, bounds: { windowState: "fullscreen" } });
    nativePlace(processId, display);
    await wait(250);
    sourceWindow = await page.call("Browser.getWindowForTarget");
    if (windowCoversDisplay(sourceWindow?.bounds, display)) return sourceWindow;
  }
  return sourceWindow;
}

export function webRtcDiagnosticProgress(samples) {
  if (samples.length < 2) return false;
  const first = samples[0], last = samples.at(-1);
  return samples.every(sample => /^(host|srflx|prflx|relay)->(host|srflx|prflx|relay)$/.test(sample.route ?? "") &&
    /^avc1\./i.test(sample.worker?.codec ?? "")) &&
    last.framesDrawn > first.framesDrawn &&
    Number.isFinite(last.renderedAgeMs) && last.renderedAgeMs < 1000 &&
    Number.isFinite(last.glassToGlass?.ageMs) && last.glassToGlass.ageMs < 1000 &&
    timestampReached(last.glassToGlass?.timestamp, ((first.glassToGlass?.timestamp ?? 0) + 1) >>> 0);
}

export function normalizeDecoderPath(decoderPath) {
  return decoderPath === "native" ? "native" : "webcodecs";
}

/** Session receiver options carry the decoder path so a reconnect cannot
 * silently fall back to the diagnostic WebCodecs decoder. Partial options
 * inherit the current session's decoder path unless they set one. */
export function normalizeReceiverOptions(options = {}, current = {}) {
  return {
    relay: options.relay !== false,
    adaptive: options.adaptive === true,
    decoderPath: normalizeDecoderPath(options.decoderPath ?? current.decoderPath),
  };
}

export function sameReceiverOptions(a = {}, b = {}) {
  return a.relay === b.relay && a.adaptive === b.adaptive &&
    normalizeDecoderPath(a.decoderPath) === normalizeDecoderPath(b.decoderPath);
}

export function buildReceiverUrl(baseUrl, transport, options = {}) {
  if (!["wss", "webrtc"].includes(transport)) throw new RangeError("transport must be wss or webrtc");
  const url = new URL(baseUrl);
  url.searchParams.set("transport", transport);
  url.searchParams.set("quality", options.adaptive ? "auto" : "high");
  url.searchParams.set("adaptive", options.adaptive ? "1" : "0");
  url.searchParams.set("diag", "1");
  url.searchParams.set("profile", "1");
  // Keep the session's native RTC preference across reconnects; WSS always
  // decodes through WebCodecs regardless of that retained preference.
  url.searchParams.set("decoderPath", transport === "wss" ? "webcodecs" : normalizeDecoderPath(options.decoderPath));
  if (options.labRun) {
    url.searchParams.set("labRun", options.labRun);
    url.searchParams.set("labCompat", STREAM_LAB_COMPATIBILITY.id);
  }
  if (transport === "webrtc" && options.relay !== false) url.searchParams.set("ice", "relay");
  return url;
}

export function buildLabHostUrl(baseUrl, runId) {
  const url = new URL(baseUrl);
  url.protocol = url.protocol === "http:" ? "ws:" : "wss:";
  url.pathname = "/lab-host-ws";
  url.search = "";
  url.hash = "";
  url.searchParams.set("run", runId);
  url.searchParams.set("compat", STREAM_LAB_COMPATIBILITY.id);
  return url;
}

export function buildCandidateHostLaunch(executable, baseUrl, runId, profilePath, options = {}) {
  const env = {
    BUBBLEPLAY_LAB_IDR_TELEMETRY: "1",
    BUBBLEPLAY_LAB_RESET_ENCODER_ON_DIAGNOSTIC_KEYFRAME:
      options.resetEncoderOnQualityKeyframe === false ? "0" : "1",
  };
  if (options.workerVersionOverride) {
    const override = workerVersionOverrideFromEnv({
      BUBBLEPLAY_WORKER_NAME: options.workerVersionOverride.workerName,
      BUBBLEPLAY_WORKER_VERSION_ID: options.workerVersionOverride.versionId,
    });
    env.BUBBLEPLAY_LAB_WORKER_NAME = override.workerName;
    env.BUBBLEPLAY_LAB_WORKER_VERSION_ID = override.versionId;
  }
  return {
    command: executable,
    args: ["--mode", "serve", "--worker", buildLabHostUrl(baseUrl, runId).href,
      "--benchmark-profile", profilePath],
    env,
  };
}

export function sessionStartupPlan(mode = "benchmark") {
  if (!["benchmark", "manual"].includes(mode))
    throw new RangeError("session startup mode must be benchmark or manual");
  const automated = mode === "benchmark";
  return {
    receiverStartMinimized: automated,
    placeReceiverOnCaptureDisplay: automated,
    startSyntheticSource: automated,
  };
}

export function buildReceiverChromeArgs(profilePath, port, url, options = {}) {
  const loopback = ["127.0.0.1", "::1", "localhost"].includes(new URL(url).hostname);
  return [
    `--remote-debugging-port=${port}`,
    `--user-data-dir=${profilePath}`,
    "--no-first-run",
    ...(options.startMinimized === false ? [] : ["--start-minimized"]),
    ...(loopback ? ["--disable-features=LocalNetworkAccessChecksWebSockets"] : []),
    "--disable-background-timer-throttling",
    "--disable-backgrounding-occluded-windows", "--disable-renderer-backgrounding",
    "--autoplay-policy=no-user-gesture-required", "--window-position=40,40",
    "--window-size=1940,1140", "--force-device-scale-factor=1", String(url),
  ];
}

export function receiverBootstrapUrl(targetUrl, options = {}) {
  return options.workerVersionOverride || options.receiverAssetOverride
    ? "about:blank"
    : String(targetUrl);
}

export function receiverAuthenticationReady(state, runId) {
  return state?.app === true && state?.diagnostics === true && state?.labRun === runId;
}

export function workerVersionOverrideFromEnv(env = process.env) {
  const workerName = String(env.BUBBLEPLAY_WORKER_NAME || "").trim();
  const versionId = String(env.BUBBLEPLAY_WORKER_VERSION_ID || "").trim();
  if (!workerName && !versionId) return null;
  if (!/^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$/.test(workerName))
    throw new RangeError("BUBBLEPLAY_WORKER_NAME must be a canonical Worker name");
  if (!/^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(versionId))
    throw new RangeError("BUBBLEPLAY_WORKER_VERSION_ID must be a canonical Worker version UUID");
  return { workerName, versionId: versionId.toLowerCase() };
}

export async function applyWorkerVersionOverride(receiver, override) {
  if (!override) return false;
  const validated = workerVersionOverrideFromEnv({
    BUBBLEPLAY_WORKER_NAME: override.workerName,
    BUBBLEPLAY_WORKER_VERSION_ID: override.versionId,
  });
  await receiver.call("Network.enable");
  await receiver.call("Network.setCacheDisabled", { cacheDisabled: true });
  await receiver.call("Network.setExtraHTTPHeaders", {
    headers: {
      "Cloudflare-Workers-Version-Overrides":
        `${validated.workerName}="${validated.versionId}"`,
    },
  });
  return true;
}

export function installReceiverWebSocketDiagnostics(receiver, store, override) {
  const requests = new Map();
  const expectedHeader = override
    ? `${override.workerName}="${override.versionId}"`
    : null;
  const append = event => store.appendTelemetry({ ...event, at: new Date().toISOString() });
  const pathFor = value => {
    try {
      const url = new URL(value);
      return url.pathname.startsWith("/lab-") && url.pathname.endsWith("ws") ? url.pathname : null;
    } catch {
      return null;
    }
  };
  const headerValue = headers => {
    const entry = Object.entries(headers ?? {}).find(([name]) =>
      name.toLowerCase() === "cloudflare-workers-version-overrides");
    return entry ? String(entry[1]) : null;
  };
  return [
    receiver.on("Network.webSocketCreated", event => {
      const path = pathFor(event?.url);
      if (!path) return;
      requests.set(event.requestId, path);
      append({
        phase: "receiver-websocket-created",
        requestId: String(event.requestId),
        path,
      });
    }),
    receiver.on("Network.webSocketWillSendHandshakeRequest", event => {
      const path = pathFor(event?.request?.url) ?? requests.get(event?.requestId);
      if (!path) return;
      requests.set(event.requestId, path);
      const actualHeader = headerValue(event.request.headers);
      append({
        phase: "receiver-websocket-handshake",
        requestId: String(event.requestId),
        path,
        overrideHeaderPresent: actualHeader !== null,
        overrideMatches: expectedHeader !== null && actualHeader === expectedHeader,
      });
    }),
    receiver.on("Network.webSocketHandshakeResponseReceived", event => {
      const path = requests.get(event?.requestId);
      if (!path) return;
      append({
        phase: "receiver-websocket-response",
        requestId: String(event.requestId),
        path,
        status: Number(event?.response?.status) || 0,
      });
    }),
    receiver.on("Network.webSocketFrameError", event => {
      const path = requests.get(event?.requestId);
      if (!path) return;
      append({
        phase: "receiver-websocket-error",
        requestId: String(event.requestId),
        path,
        message: String(event?.errorMessage || "unknown WebSocket error").slice(0, 256),
      });
    }),
    receiver.on("Network.webSocketClosed", event => {
      const path = requests.get(event?.requestId);
      if (!path) return;
      append({
        phase: "receiver-websocket-closed",
        requestId: String(event.requestId),
        path,
      });
      requests.delete(event.requestId);
    }),
  ];
}

export async function installReceiverAssetOverrides(receiver, options = {}) {
  const origin = new URL(options.baseUrl || DEFAULT_URL).origin;
  const indexHtml = options.indexHtml ??
    readFileSync(resolve(import.meta.dirname, "..", "public", "index.html"), "utf8");
  // Pin every local JS asset together: an extracted entrypoint must never fall
  // through to an older deployed module (or a 404) during candidate validation.
  const publicDir = resolve(import.meta.dirname, "..", "public");
  const scripts = new Map(readdirSync(publicDir).filter(name => name.endsWith(".js"))
    .map(name => [`/${name}`, readFileSync(resolve(publicDir, name), "utf8")]));
  if (options.mediaJs !== undefined) scripts.set("/media.js", options.mediaJs);
  receiver.on("Fetch.requestPaused", async event => {
    const requestUrl = new URL(event.request.url);
    const body = requestUrl.origin === origin && event.resourceType === "Document" && requestUrl.pathname === "/"
      ? indexHtml
      : requestUrl.origin === origin && event.resourceType === "Script"
        ? scripts.get(requestUrl.pathname) ?? null
        : null;
    if (body === null) {
      await receiver.call("Fetch.continueRequest", { requestId: event.requestId });
      return;
    }
    const contentType = event.resourceType === "Document"
      ? "text/html; charset=utf-8"
      : "text/javascript; charset=utf-8";
    await receiver.call("Fetch.fulfillRequest", {
      requestId: event.requestId,
      responseCode: 200,
      responseHeaders: [
        { name: "Content-Type", value: contentType },
        { name: "Cache-Control", value: "no-store" },
      ],
      body: Buffer.from(body).toString("base64"),
    });
  });
  await receiver.call("Fetch.enable", {
    patterns: [
      { urlPattern: `${origin}/*`, resourceType: "Document", requestStage: "Request" },
      ...[...scripts.keys()].map(path => ({ urlPattern: `${origin}${path}*`, resourceType: "Script", requestStage: "Request" })),
    ],
  });
  return true;
}

export function assertReceiverAssetBuild(snapshot, expected = STREAM_LAB_ASSET_BUILD) {
  const actual = snapshot?.assetBuild && typeof snapshot.assetBuild === "object"
    ? { index: snapshot.assetBuild.index ?? null, media: snapshot.assetBuild.media ?? null }
    : { index: null, media: null };
  if (actual.index !== expected || actual.media !== expected) {
    throw new StreamLabUnavailable("receiver-asset-build-mismatch",
      "The Stream Lab receiver did not load the required index and media asset build.",
      { expected, actual });
  }
  return actual;
}

function sameCompatibility(actual, expected) {
  return JSON.stringify(actual) === JSON.stringify(expected);
}

export function assertWorkerCompatibility(result, expected = STREAM_LAB_COMPATIBILITY) {
  const actual = result?.compatibility ?? null;
  if (result?.ok !== true || !sameCompatibility(actual, expected)) {
    throw new StreamLabUnavailable(
      "worker-compatibility-mismatch",
      "The pinned Worker does not implement the Stream Lab compatibility contract required by this checkout.",
      { expected, actual, response: result ?? null },
    );
  }
  return actual;
}

export function assertReceiverCompatibility(snapshot, expected = STREAM_LAB_COMPATIBILITY) {
  const assetBuild = assertReceiverAssetBuild(snapshot, expected.receiverBuild);
  const actual = snapshot?.compatibility ?? null;
  if (!sameCompatibility(actual, expected)) {
    throw new StreamLabUnavailable(
      "receiver-compatibility-mismatch",
      "The Stream Lab receiver assets do not implement the compatibility contract required by this checkout.",
      { expected, actual, assetBuild },
    );
  }
  return { assetBuild, compatibility: actual };
}

export function classifyCandidateProfileFailure(diagnostics) {
  if (diagnostics?.signaling?.benchStatusRejection) return "bench-status-rejected";
  if (Number(diagnostics?.wssMedia?.packetRejects?.total) > 0) return "media-packet-rejected";
  if (Number(diagnostics?.wssMedia?.receivedPackets) > 0 &&
      Number(diagnostics?.wssMedia?.reconstructed) === 0)
    return "wss-startup-no-reconstruction";
  return "candidate-profile-timeout";
}

export function buildCandidateProfileLease(profile, runId, nowMs = Date.now(), applyRevision = 1) {
  return { ...profile, runId, expiresAtMs: nowMs + 600_000, applyRevision };
}

function publishCandidateProfile(path, lease) {
  const temporary = `${path}.${process.pid}.${Date.now()}.tmp`;
  writeFileSync(temporary, `${JSON.stringify(lease)}\n`, { flag: "wx" });
  renameSync(temporary, path);
}

export function effectiveEncoderBitrateMbps(requestedMbps, bandwidthMbps, transport = "wss") {
  if (!Number.isFinite(bandwidthMbps)) return requestedMbps;
  const headroom = transport === "wss" ? 0.65 : 0.90;
  return Math.min(requestedMbps, Math.floor(bandwidthMbps * headroom * 10) / 10);
}

export function initialEncoderTargetMbps(requestedMbps, bandwidthMbps) {
  if (!Number.isFinite(bandwidthMbps)) return requestedMbps;
  if (bandwidthMbps >= requestedMbps) return requestedMbps;
  return clampBitrate(Math.min(requestedMbps, bandwidthMbps / 1.35));
}

export function nextStabilizedBitrateMbps(currentMbps, offeredWireMbps, bandwidthMbps) {
  if (![currentMbps, offeredWireMbps, bandwidthMbps].every(Number.isFinite) || offeredWireMbps <= 0)
    throw new RangeError("stabilization rates must be positive finite numbers");
  const adjusted = currentMbps * (0.90 * bandwidthMbps / offeredWireMbps);
  return clampBitrate(Math.min(adjusted, currentMbps * 1.8));
}

function clampBitrate(value) {
  return Math.round(Math.max(0.5, value) * 10) / 10;
}

export function selectHostMonitor(cards, display) {
  const normalizedDisplay = display.deviceName.toLowerCase();
  const displayToken = display.deviceName.replace(/^\\\\\.\\/, "").toLowerCase();
  const exact = cards.filter(card => String(card.selector || "").toLowerCase() === normalizedDisplay ||
    String(card.selector || "").toLowerCase().includes(displayToken));
  if (exact.length === 1) return exact[0];
  const virtual = cards.filter(card => !card.selected && /parsec|virtual|idd|bubbleplay/i.test(`${card.name} ${card.selector}`) && card.width === 1920 && card.height === 1080);
  if (virtual.length === 1) return virtual[0];
  const geometry = cards.filter(card => !card.selected && card.width === display.width && card.height === display.height);
  if (geometry.length === 1) return geometry[0];
  throw new StreamLabUnavailable("monitor-ambiguous", "The host monitor list could not identify the transient display unambiguously.", cards);
}

export function virtualMonitorSelectionCurrent(snapshot, display, selectedRegisteredAtMs) {
  const signaling = snapshot?.signaling;
  const registeredAtMs = Number(signaling?.registeredAtMs);
  const selectedAtMs = Number(selectedRegisteredAtMs);
  const target = String(display?.deviceName || "").toLowerCase();
  if (signaling?.registered !== true || !target || !Number.isFinite(registeredAtMs) ||
      !Number.isFinite(selectedAtMs) || registeredAtMs !== selectedAtMs) return false;
  const exact = (signaling.monitors ?? []).filter(monitor =>
    String(monitor?.selector || "").toLowerCase() === target);
  return exact.length === 1 && exact[0].selected === true;
}

export function selectPersistentVirtualDisplay(displays, cards) {
  const virtual = cards.filter(card => /parsec|virtual|idd|bubbleplay/i.test(`${card.name} ${card.selector}`) &&
    card.width === 1920 && card.height === 1080);
  if (virtual.length === 0)
    throw new StreamLabUnavailable("persistent-vdd-unavailable", "The Broker-owned Parsec virtual display is unavailable.", cards);
  if (virtual.length !== 1)
    throw new StreamLabUnavailable("persistent-vdd-ambiguous", `Expected one persistent virtual display; found ${virtual.length}.`, virtual);
  const selector = String(virtual[0].selector || "").toLowerCase();
  const matches = displays.filter(display => String(display.deviceName || "").toLowerCase() === selector);
  if (matches.length !== 1)
    throw new StreamLabUnavailable("persistent-vdd-screen-missing", "The persistent virtual monitor is not attached to the interactive desktop.",
      { monitor: virtual[0], displays });
  const display = matches[0];
  if (display.primary || display.width !== 1920 || display.height !== 1080)
    throw new StreamLabUnavailable("persistent-vdd-geometry", "The persistent virtual display must be a non-primary 1920×1080 screen.", display);
  return display;
}

export function benchmarkProfile(candidate, transport = "wss") {
  const streamMode = candidate.streamMode === "game" ? "game" : "work";
  if (candidate.codec && candidate.codec !== "hevc")
    throw new RangeError("benchmark helper profiles must use hevc; release the override before compatibility fallback");
  const v2 = candidate.schemaVersion === 2 || Object.hasOwn(candidate, "implementationVariant") ||
    Object.hasOwn(candidate, "experimentId") || Object.hasOwn(candidate, "aqMode") ||
    Object.hasOwn(candidate, "lookaheadFrames") || Object.hasOwn(candidate, "weightedPrediction") ||
    Object.hasOwn(candidate, "dirtyQpMap") || Object.hasOwn(candidate, "main10") ||
    Object.hasOwn(candidate, "roiMode") || Object.hasOwn(candidate, "resourceMode") ||
    Object.hasOwn(candidate, "asyncDepth") || Object.hasOwn(candidate, "hybridMode") ||
    Object.hasOwn(candidate, "hybridCacheMode") || Object.hasOwn(candidate, "rateControlMode");
  if (v2) return benchmarkProfileV2(candidate, transport, streamMode);
  return {
    schemaVersion: 1,
    transport,
    codec: "hevc",
    streamMode,
    adaptive: false,
    width: 1920,
    height: 1080,
    fps: candidate.fps ?? (streamMode === "game" ? 60 : 30),
    bitrateBps: Math.round((candidate.bitrateMbps ?? 6) * 1_000_000),
    gopSeconds: candidate.gopSeconds ?? (streamMode === "game" ? 1 : 2),
    encoder: {
      preset: candidate.preset,
      tuning: candidate.tuning,
      multipass: candidate.multipass,
      aqStrength: candidate.aqStrength,
      temporalAq: candidate.temporalAq,
      vbvFrames: candidate.vbvFrames,
    },
  };
}

function benchmarkProfileV2(candidate, transport, streamMode) {
  const implementationVariant = candidate.implementationVariant ?? "production";
  const experimentId = candidate.experimentId ?? "production";
  const lookaheadDepth = candidate.lookaheadFrames ?? candidate.lookaheadDepth ?? 0;
  const bitrateMbps = candidate.bitrateMbps ?? 6;
  const requestedBitrateMbps = candidate.requestedBitrateMbps ?? bitrateMbps;
  const totalMediaCapMbps = candidate.totalMediaCapMbps ?? bitrateMbps;
  const gopSeconds = candidate.gopSeconds ?? (streamMode === "game" ? 1 : 2);
  const dirtyQpMap = candidate.dirtyQpMap ?? false;
  const main10 = candidate.main10 ?? false;
  const roiMode = candidate.roiMode ?? (dirtyQpMap ? "damage" : "off");
  const resourceMode = candidate.resourceMode ?? "per-frame";
  const asyncDepth = candidate.asyncDepth ?? 0;
  const hybridMode = candidate.hybridMode ?? "off";
  const hybridCacheMode = candidate.hybridCacheMode ?? "suppress-reactivate";
  const rateControlMode = candidate.rateControlMode ?? "cbr";
  if (!["production", "candidate"].includes(implementationVariant)) throw new RangeError("implementationVariant must be production or candidate");
  if (!/^[A-Za-z0-9._-]{1,64}$/.test(experimentId)) throw new RangeError("experimentId must be 1–64 safe ASCII characters");
  if (!/^p[1-7]$/.test(candidate.preset ?? "")) throw new RangeError("preset must be p1 through p7");
  if (!["disabled", "quarter", "full"].includes(candidate.multipass)) throw new RangeError("multipass must be disabled, quarter, or full");
  if (!["none", "spatial", "temporal"].includes(candidate.aqMode)) throw new RangeError("aqMode must be none, spatial, or temporal");
  if (!Number.isInteger(candidate.aqStrength) || candidate.aqStrength < 1 || candidate.aqStrength > 15) throw new RangeError("aqStrength must be from 1 to 15");
  if (!Number.isInteger(candidate.vbvFrames) || candidate.vbvFrames < 1 || candidate.vbvFrames > 4) throw new RangeError("vbvFrames must be from 1 to 4");
  const lookaheadLimit = streamMode === "game" ? 1 : 2;
  if (!Number.isInteger(lookaheadDepth) || lookaheadDepth < 0 || lookaheadDepth > lookaheadLimit)
    throw new RangeError(`${streamMode} lookahead must be from 0 to ${lookaheadLimit}`);
  if (typeof candidate.weightedPrediction !== "boolean") throw new RangeError("weightedPrediction must be boolean");
  if (typeof dirtyQpMap !== "boolean") throw new RangeError("dirtyQpMap must be boolean");
  if (typeof main10 !== "boolean") throw new RangeError("main10 must be boolean");
  if (!["off", "damage", "text-edge", "feedback", "feedback-headroom"].includes(roiMode))
    throw new RangeError("roiMode must be off, damage, text-edge, feedback, or feedback-headroom");
  if (!["per-frame", "persistent"].includes(resourceMode))
    throw new RangeError("resourceMode must be per-frame or persistent");
  if (![0, 1].includes(asyncDepth)) throw new RangeError("asyncDepth must be 0 or 1");
  if (!["cbr", "external-budget-vbr"].includes(rateControlMode))
    throw new RangeError("rateControlMode must be cbr or external-budget-vbr");
  if (rateControlMode === "cbr" && (candidate.targetQuality !== undefined || candidate.peakBitrateMultiplier !== undefined))
    throw new RangeError("CBR cannot carry VBR quality controls");
  if (rateControlMode === "external-budget-vbr" &&
      (candidate.targetQuality !== 23 || candidate.peakBitrateMultiplier !== 4 || candidate.vbvFrames !== 4))
    throw new RangeError("external-budget VBR is bounded to CQ23, a 4x ceiling, and four-frame VBV");
  if (!["off", "png-static-roi", "png-quiescent-detail", "hevc-quiescent-tiles"].includes(
    hybridMode))
    throw new RangeError("hybridMode must be off, png-static-roi, png-quiescent-detail, or hevc-quiescent-tiles");
  if (!["dda-gated", "suppress-reactivate"].includes(hybridCacheMode))
    throw new RangeError("hybridCacheMode must be dda-gated or suppress-reactivate");
  if (hybridCacheMode === "dda-gated" && hybridMode !== "png-quiescent-detail")
    throw new RangeError("hybridCacheMode dda-gated requires hybridMode png-quiescent-detail");
  if (dirtyQpMap && !["damage"].includes(roiMode))
    throw new RangeError("legacy dirtyQpMap can only normalize to roiMode damage");
  if (main10 && (implementationVariant !== "candidate" || transport !== "wss" || streamMode !== "work" ||
      (candidate.fps ?? 30) !== 30))
    throw new RangeError("main10 is limited to explicit 1080p30 WSS candidate profiles");
  if (!Number.isFinite(bitrateMbps) || bitrateMbps < 0.5 || bitrateMbps > 12) throw new RangeError("effective bitrateMbps must be from 0.5 to 12");
  if (!Number.isFinite(requestedBitrateMbps) || requestedBitrateMbps < 0.5 || requestedBitrateMbps > 12)
    throw new RangeError("requestedBitrateMbps must be from 0.5 to 12");
  if (!Number.isFinite(totalMediaCapMbps) || totalMediaCapMbps < 0.5 || totalMediaCapMbps > 12)
    throw new RangeError("totalMediaCapMbps must be from 0.5 to 12");
  if (![1, 2, 4].includes(gopSeconds)) throw new RangeError("gopSeconds must be 1, 2, or 4");
  const roiActive = roiMode !== "off";
  const hybridActive = hybridMode !== "off";
  const resourceActive = resourceMode !== "per-frame" || asyncDepth !== 0;
  const rateControlActive = rateControlMode !== "cbr";
  const activeAxes = Number(roiActive) + Number(hybridActive) + Number(resourceActive) + Number(rateControlActive);
  if (activeAxes > 0) {
    if (activeAxes !== 1 || implementationVariant !== "candidate" || transport !== "wss" ||
        streamMode !== "work" || (candidate.fps ?? 30) !== 30 || requestedBitrateMbps !== 6 ||
        totalMediaCapMbps !== 6 || bitrateMbps > requestedBitrateMbps || bitrateMbps > totalMediaCapMbps ||
        main10 || (asyncDepth === 1 && resourceMode !== "persistent"))
      throw new RangeError("pipeline experiment controls require one candidate axis on the 1080p30 6 Mbps WSS control");
  }
  if (main10 && activeAxes > 0)
    throw new RangeError("Main10 combinations are deferred until a pipeline experiment wins");
  return {
    schemaVersion: 2,
    implementationVariant,
    experimentId,
    transport,
    codec: "hevc",
    streamMode,
    adaptive: false,
    width: 1920,
    height: 1080,
    fps: candidate.fps ?? (streamMode === "game" ? 60 : 30),
    bitrateBps: Math.round(bitrateMbps * 1_000_000),
    ...(candidate.requestedBitrateMbps !== undefined
      ? { requestedBitrateBps: Math.round(requestedBitrateMbps * 1_000_000) } : {}),
    ...(candidate.totalMediaCapMbps !== undefined
      ? { totalMediaCapBps: Math.round(totalMediaCapMbps * 1_000_000) } : {}),
    gopSeconds,
    hybridMode,
    hybridCacheMode,
    encoder: {
      preset: candidate.preset,
      tuning: candidate.tuning ?? "ultra-low-latency",
      multipass: candidate.multipass,
      aqMode: candidate.aqMode,
      aqStrength: candidate.aqStrength,
      vbvFrames: candidate.vbvFrames,
      lookaheadDepth,
      weightedPrediction: candidate.weightedPrediction,
      dirtyQpMap,
      main10,
      roiMode,
      resourceMode,
      asyncDepth,
      ...(rateControlMode !== "cbr" ? {
        rateControlMode, targetQuality: candidate.targetQuality,
        peakBitrateMultiplier: candidate.peakBitrateMultiplier,
      } : {}),
    },
  };
}

export function benchStatusConfirmsApply(previous, current, expected, runId) {
  if (!current || ![1, 2].includes(current.protocol) || current.runId !== runId || !["applied", "sample"].includes(current.phase)) return false;
  if (expected.schemaVersion === 2 && current.protocol !== 2) return false;
  if (benchTransport(current) !== expected.transport || !benchMatches(current, expected)) return false;
  if (!Number.isSafeInteger(current.profileRevision)) return false;
  if (!previous) return true;
  if (!Number.isSafeInteger(previous.profileRevision)) return false;
  return current.profileRevision > previous.profileRevision;
}

export function summarizePerformance(samples, context = {}) {
  if (samples.length < 2) throw new StreamLabUnavailable("insufficient-telemetry", "At least two performance samples are required.");
  const first = samples[0];
  const last = samples.at(-1);
  const durationSeconds = Math.max(0.001, (last.elapsedMs - first.elapsedMs) / 1000);
  const bench = sumBenchWindows(samples);
  const encodedBytes = bench.encodedBytes;
  const offeredWireBytes = bench.wssPayloadBytes;
  const encodeCount = bench.encodeCount;
  const encodeUs = bench.encodeUsTotal;
  const frameDelta = nonnegativeDelta(frameCount(last), frameCount(first));
  const presentationFirst = first.wssMedia?.presentation;
  const presentationLast = last.wssMedia?.presentation;
  const presentationCounterFields = [
    "immediateAttempts", "immediatePresented", "rafEnqueued", "rafCoalesced", "rafLateDrops",
  ];
  const presentationCountersValid = presentationFirst?.protocol === 1 &&
    presentationLast?.protocol === 1 && presentationCounterFields.every(field =>
      Number.isFinite(Number(presentationFirst[field])) && Number.isFinite(Number(presentationLast[field])));
  const wssPresentation = presentationCountersValid ? {
    protocol: 1,
    decodedFrames: nonnegativeDelta(last.framesReceived, first.framesReceived),
    presentedFrames: frameDelta,
    ...Object.fromEntries(presentationCounterFields.map(field => [
      field,
      nonnegativeDelta(presentationLast[field], presentationFirst[field]),
    ])),
  } : null;
  const latency = samples.map(sample => sample.glassToGlass?.ageMs).filter(validLatency);
  const memory = samples.map(sample => sample.system?.privateMemoryBytes).filter(Number.isFinite);
  const queueFirst = Number(first.queued) || 0;
  const queueLast = Number(last.queued) || 0;
  const resetFirst = Number(first.resyncs ?? first.worker?.resyncs) || 0;
  const resetLast = Number(last.resyncs ?? last.worker?.resyncs) || 0;
  const dependencyFirst = Number(first.worker?.dependencyLosses) || 0;
  const dependencyLast = Number(last.worker?.dependencyLosses) || 0;
  const dimensions = samples.map(sample => `${sample.hostQuality?.width ?? "?"}x${sample.hostQuality?.height ?? "?"}@${sample.hostQuality?.fps ?? "?"}`);
  const transitions = dimensions.slice(1).filter((value, index) => value !== dimensions[index]).length;
  const maxFrameGap = Math.max(0, ...samples.map(sample => Number(sample.maxFrameGap ?? sample.nativePresentation?.maxFrameGapMs) || 0));
  const frameGaps = samples.map(sample => Number(sample.maxFrameGap ?? sample.nativePresentation?.maxFrameGapMs)).filter(Number.isFinite);
  const encodedWindows = uniqueBenchWindows(samples).map(window => windowBitrateMbps(window, "encodedBytes")).filter(Number.isFinite);
  const wireWindows = uniqueBenchWindows(samples).map(window => windowBitrateMbps(window, "wssPayloadBytes")).filter(Number.isFinite);
  const systems = samples.map(sample => sample.system).filter(Boolean);
  const firstCpu = systems.find(system => Number.isFinite(system.cpuSeconds));
  const lastCpu = systems.findLast(system => Number.isFinite(system.cpuSeconds));
  const privateMemoryMb = systems.map(system => Number(system.privateMemoryBytes) / 1024 / 1024).filter(Number.isFinite);
  const gpu = systems.map(system => Number(system.gpuUtilizationPct)).filter(Number.isFinite);
  const encoderGpu = systems.map(system => Number(system.encoderUtilizationPct)).filter(Number.isFinite);
  const vram = systems.map(system => Number(system.vramUsedMb)).filter(Number.isFinite);
  const power = systems.map(system => Number(system.powerWatts)).filter(Number.isFinite);
  const stageTraces = [];
  const seenStageTraces = new Set();
  for (const sample of samples) {
    const trace = sample.frameTrace?.trace ?? sample.frameTrace;
    if (!trace?.network) continue;
    const key = `${trace.network.sequence ?? "?"}:${trace.presentation?.presentedAtMs ?? "?"}`;
    if (seenStageTraces.has(key)) continue;
    seenStageTraces.add(key);
    stageTraces.push(trace);
  }
  const stageDuration = (start, end) => stageTraces
    .map(trace => Number(end(trace)) - Number(start(trace)))
    .filter(value => Number.isFinite(value) && value >= 0);
  const sourceToPresented = stageTraces
    .map(trace => Number(trace.presentation?.sourceToPresentedMs))
    .filter(value => Number.isFinite(value) && value >= 0);
  const decode = stageDuration(
    trace => trace.decode?.queuedAtMs,
    trace => trace.decode?.decodedAtMs,
  );
  const presentation = stageDuration(
    trace => trace.decode?.decodedAtMs,
    trace => trace.presentation?.presentedAtMs,
  );
  const receiveToPresent = stageDuration(
    trace => trace.network?.receivedAtMs,
    trace => trace.presentation?.presentedAtMs,
  );
  const hostDurationSeconds = bench.windowDurationMs > 0 ? bench.windowDurationMs / 1000 : durationSeconds;
  const receiverFirst = Number(first.wssMedia?.receivedBytes);
  const receiverLast = Number(last.wssMedia?.receivedBytes);
  const deliveredWireBitrateMbps = Number.isFinite(receiverFirst) && Number.isFinite(receiverLast)
    ? Math.max(0, receiverLast - receiverFirst) * 8 / durationSeconds / 1_000_000 : null;
  const offeredWireBitrateMbps = offeredWireBytes * 8 / hostDurationSeconds / 1_000_000;
  const encodedBitrateMbps = encodedBytes * 8 / hostDurationSeconds / 1_000_000;
  return {
    transport: context.transport ?? bench.transport ?? last.benchStatus?.transport ?? "wss",
    codec: "hevc",
    firstFrameMs: context.firstFrameMs ?? null,
    presentedFps: frameDelta / durationSeconds,
    encodeP95Ms: bench.encodeUsMax / 1000,
    encodeMeanMs: encodeCount ? encodeUs / encodeCount / 1000 : null,
    latencyP95Ms: percentile(latency, 0.95),
    latencyP50Ms: percentile(latency, 0.50),
    performanceTraceSamples: stageTraces.length,
    sourceToPresentedP50Ms: percentile(sourceToPresented, 0.50),
    sourceToPresentedP95Ms: percentile(sourceToPresented, 0.95),
    decodeP50Ms: percentile(decode, 0.50),
    decodeP95Ms: percentile(decode, 0.95),
    presentationP50Ms: percentile(presentation, 0.50),
    presentationP95Ms: percentile(presentation, 0.95),
    wssPresentation,
    receiveToPresentP50Ms: percentile(receiveToPresent, 0.50),
    receiveToPresentP95Ms: percentile(receiveToPresent, 0.95),
    actionToPhotonP95Ms: percentile(context.actionToPhotonSamples ?? [], 0.95),
    encodedBitrateMbps,
    offeredWireBitrateMbps,
    deliveredWireBitrateMbps,
    wireBitrateMbps: deliveredWireBitrateMbps,
    wireExpansion: encodedBytes > 0 ? offeredWireBytes / encodedBytes : null,
    bandwidthVerified: verifyBandwidth(deliveredWireBitrateMbps, context.bandwidthMbps, context.qosPolicy),
    qosPolicy: context.qosPolicy ?? null,
    idrCount: bench.idrCount,
    deltaCount: bench.deltaCount,
    skips: bench.skips,
    qpMapFrames: bench.qpMapFrames,
    qpMapChangedCtbs: bench.qpMapChangedCtbs,
    qpMapTotalCtbs: bench.qpMapTotalCtbs,
    roiActivity: bench.roiActivity ?? null,
    hostHybridActivity: bench.hybridActivity ?? null,
    encodeStages: summarizeBenchEncodeStages(bench.encodeStages),
    freezes: maxFrameGap > 1_000 ? 1 : 0,
    frameGapP95Ms: percentile(frameGaps, 0.95),
    frameGapMaxMs: maxFrameGap,
    encodedBitrateCv: coefficientOfVariation(encodedWindows),
    wireBitrateCv: coefficientOfVariation(wireWindows),
    hostCpuPct: firstCpu && lastCpu ? Math.max(0, Number(lastCpu.cpuSeconds) - Number(firstCpu.cpuSeconds)) / durationSeconds * 100 : null,
    gpuUtilizationMeanPct: mean(gpu),
    gpuEncoderMeanPct: mean(encoderGpu),
    privateMemoryPeakMb: privateMemoryMb.length ? Math.max(...privateMemoryMb) : null,
    vramPeakMb: vram.length ? Math.max(...vram) : null,
    gpuPowerMeanWatts: mean(power),
    decoderResets: Math.max(0, resetLast - resetFirst),
    dependencyLosses: Math.max(0, dependencyLast - dependencyFirst),
    spatialTransitions: transitions,
    queueSlopePerMinute: (queueLast - queueFirst) / durationSeconds * 60,
    memorySlopeMbPerMinute: memory.length > 1 ? (memory.at(-1) - memory[0]) / 1024 / 1024 / durationSeconds * 60 : 0,
    sampleCount: samples.length,
    durationSeconds,
  };
}

export function aggregateQualityRepetitions(repetitions, id = "quality") {
  if (!Array.isArray(repetitions) || repetitions.length === 0)
    throw new RangeError("quality repetitions must contain at least one capture");
  const fields = [
    "ocrCer", "worstTileSsim", "p5TileSsim", "vmaf", "ssim", "psnr", "blur", "block",
    "vmafMean", "vmafP5", "ssimMean", "ssimP5", "edgeFidelity", "colorError",
    "unchangedRegionError", "blockArtifactSeverity", "presentationCadenceP95Ms",
  ];
  const aggregate = {
    id,
    result: "complete",
    repetitionCount: repetitions.length,
    repetitions,
    noise: {},
  };
  for (const field of fields) {
    const values = repetitions.map(item => item[field]).filter(Number.isFinite);
    aggregate[field] = median(values);
    aggregate.noise[field] = medianAbsoluteDeviation(values);
  }
  aggregate.qualitySamples = repetitions.reduce((total, item) => total + (Number(item.qualitySamples) || 0), 0);
  aggregate.frameCount = repetitions.reduce((total, item) => total + (Number(item.frameCount) || 0), 0);
  aggregate.exactFrameRatio = Math.min(...repetitions.map(item => Number(item.exactFrameRatio)).filter(Number.isFinite));
  for (const field of ["duplicateFrames", "droppedFrames", "freezeCount"])
    aggregate[field] = Math.max(0, ...repetitions.map(item => Number(item[field]) || 0));
  aggregate.sourceParityLoss = summarizeSourceParityLoss(aggregate);
  return aggregate;
}

export async function collectQualityRepetitions(session, candidateId, sceneIds, options = {}) {
  const repetitions = Number(options.repetitions ?? 3);
  if (!Number.isSafeInteger(repetitions) || repetitions < 1 || repetitions > 10)
    throw new RangeError("quality repetitions must be an integer from 1 to 10");
  const { repetitions: _repetitions, ...captureOptions } = options;
  const captures = [];
  for (let repetition = 1; repetition <= repetitions; repetition++)
    captures.push(await session.collectQuality(`${candidateId}-r${repetition}`, sceneIds, captureOptions));
  return aggregateQualityRepetitions(captures, candidateId);
}

export async function prepareQualitySourceFrame(source, scene, targetFrame, options = {}) {
  const poller = options.poller ?? poll;
  await source.evaluate(
    `window.__bubbleplaySource.setPausedScene(` +
    `${JSON.stringify(scene)},469,${JSON.stringify(targetFrame)}); true`,
  );
  const metadata = await poller(async () => {
    const value = await source.evaluate("window.__bubbleplaySource.metadata()");
    return value.paused && value.frameId === targetFrame ? value : false;
  }, options.timeoutMs ?? 3_000);
  if (!metadata) return null;
  const painted = await source.evaluate(`new Promise(resolve => requestAnimationFrame(() =>
    requestAnimationFrame(() => resolve(window.__bubbleplaySource.metadata()))))`);
  return painted?.paused === true && painted.scene === metadata.scene &&
    painted.frameId === metadata.frameId && painted.generation === metadata.generation &&
    painted.epochMs === metadata.epochMs ? painted : null;
}

export async function requestCleanQualityKeyframe(receiver, requestId, options = {}) {
  const label = options.label ?? "quality frame";
  const recoveryRequested = await receiver.evaluate(
    `window.__bubbleplayDiagnostics?.injectDecoderFault?.(` +
    `"stream-lab-quality-capture", ${JSON.stringify(requestId)}) ?? false`,
  );
  if (!recoveryRequested)
    throw new StreamLabUnavailable("quality-keyframe-request",
      `The receiver could not request a clean keyframe for ${label}.`);
  const poller = options.poller ?? poll;
  const qualityAck = await poller(() => receiver.evaluate(
    `window.__bubbleplayDiagnostics?.qualityKeyframeAck?.(${JSON.stringify(requestId)}) ?? null`,
  ), options.timeoutMs ?? 5_000);
  if (!qualityAck || !Number.isSafeInteger(qualityAck.sequence))
    throw new StreamLabUnavailable("quality-keyframe-ack-timeout",
      `The host did not acknowledge the normalized quality keyframe for ${label}.`);
  return qualityAck;
}

export function armPresentedIdentityHistory(receiver, frameCount = 16) {
  if (!Number.isInteger(frameCount) || frameCount < 1 || frameCount > 64)
    throw new RangeError("frameCount must be an integer between 1 and 64");
  return receiver.evaluate(
    `window.__bubbleplayDiagnostics?.armPresentedIdentityHistory?.(${frameCount}) ?? false`,
  );
}

export async function prepareQualityDeltaFrame(source, anchor, options = {}) {
  if (typeof options.waitForTrigger !== "function")
    return advanceQualityDeltaFrame(source, anchor, options);
  const triggerFrames = options.triggerFrames ?? 1;
  if (!Number.isInteger(triggerFrames) || triggerFrames < 1 || triggerFrames > 16)
    throw new RangeError("triggerFrames must be an integer from 1 to 16");
  let previous = anchor;
  for (let index = 0; index < triggerFrames; index += 1) {
    const trigger = await advanceQualityDeltaFrame(source, previous, options);
    if (!trigger || await options.waitForTrigger(trigger, index) !== true) return null;
    previous = trigger;
  }
  return advanceQualityDeltaFrame(source, previous, options);
}

export function qualityDeltaTriggerFrames(options = {}) {
  return options.deltaTriggerFrames ?? 3;
}

async function advanceQualityDeltaFrame(source, anchor, options = {}) {
  const anchorFrame = Number(anchor?.frameId);
  if (!Number.isSafeInteger(anchorFrame) || anchorFrame < 0 ||
      typeof anchor?.scene !== "string" || !Number.isSafeInteger(anchor?.generation)) return null;
  const targetFrame = anchorFrame + 1;
  const poller = options.poller ?? poll;
  await source.evaluate(`window.__bubbleplaySource.pauseAt(${JSON.stringify(targetFrame)}); true`);
  const metadata = await poller(async () => {
    const value = await source.evaluate("window.__bubbleplaySource.metadata()");
    return value.paused && value.scene === anchor.scene && value.generation === anchor.generation &&
      value.frameId === targetFrame ? value : false;
  }, options.timeoutMs ?? 3_000);
  if (!metadata) return null;
  const painted = await source.evaluate(`new Promise(resolve => requestAnimationFrame(() =>
    requestAnimationFrame(() => resolve(window.__bubbleplaySource.metadata()))))`);
  return painted?.paused === true && painted.scene === metadata.scene &&
    painted.frameId === metadata.frameId && painted.generation === metadata.generation &&
    painted.epochMs === metadata.epochMs ? painted : null;
}

export function qualitySampleStem(scene, metadata) {
  return `${scene}-${metadata.frameId}`;
}

export function combinePerformanceAndQuality(performance, quality) {
  const combined = {
    ...performance,
    ...quality,
    // Quality screenshots are intentionally seconds apart and their WebCodecs
    // timestamps are microseconds. Cadence/freeze gates must come exclusively
    // from the uninterrupted performance pass.
    presentationCadenceP95Ms: performance.frameGapP95Ms,
    freezeCount: performance.freezes,
  };
  return { ...combined, sourceParityLoss: summarizeSourceParityLoss(combined) };
}

function uniqueBenchWindows(samples) {
  const windows = new Map();
  for (const sample of samples) {
    const status = sample.benchStatus;
    if (!status || !Number.isSafeInteger(status.sequence)) continue;
    const key = `${status.runId ?? "unknown"}:${status.sequence}`;
    if (!windows.has(key)) windows.set(key, benchMetrics(status));
  }
  return [...windows.values()];
}

function mean(values) { return values.length ? values.reduce((sum, value) => sum + value, 0) / values.length : null; }
function windowBitrateMbps(window, field) {
  const bytes = Number(window[field] ?? (field === "wssPayloadBytes" ? window.wireBytes : null));
  const durationMs = Number(window.windowDurationMs);
  if (!Number.isFinite(bytes) || bytes < 0) return null;
  return bytes * 8 / Math.max(0.001, Number.isFinite(durationMs) && durationMs > 0 ? durationMs / 1000 : 1) / 1_000_000;
}
function coefficientOfVariation(values) {
  const average = mean(values);
  if (!Number.isFinite(average) || average === 0 || values.length < 2) return null;
  const variance = values.reduce((sum, value) => sum + (value - average) ** 2, 0) / values.length;
  const coefficient = Math.sqrt(variance) / average;
  return coefficient < Number.EPSILON ? 0 : coefficient;
}

function sumBenchWindows(samples) {
  const windows = new Map();
  for (const sample of samples) {
    const status = sample.benchStatus;
    if (!status || !Number.isSafeInteger(status.sequence)) continue;
    const key = `${status.runId ?? "unknown"}:${status.sequence}`;
    if (!windows.has(key)) windows.set(key, benchMetrics(status));
  }
  const total = {
    transport: null, encodedBytes: 0, wssPayloadBytes: 0, encodeCount: 0,
    encodeUsTotal: 0, encodeUsMax: 0, idrCount: 0, deltaCount: 0, skips: 0, windowDurationMs: 0,
    qpMapFrames: 0, qpMapChangedCtbs: 0, qpMapTotalCtbs: 0,
    roiActivity: null, encodeStages: null, hybridActivity: null,
  };
  for (const window of windows.values()) {
    total.transport = window.transport ?? total.transport;
    for (const field of ["encodedBytes", "encodeCount", "encodeUsTotal", "idrCount", "deltaCount", "skips",
      "qpMapFrames", "qpMapChangedCtbs", "qpMapTotalCtbs"]) {
      const value = Number(window[field]);
      if (Number.isFinite(value) && value >= 0) total[field] += value;
    }
    const wire = Number(window.wssPayloadBytes ?? window.wireBytes);
    if (Number.isFinite(wire) && wire >= 0) total.wssPayloadBytes += wire;
    const windowDurationMs = Number(window.windowDurationMs);
    if (Number.isFinite(windowDurationMs) && windowDurationMs > 0) total.windowDurationMs += windowDurationMs;
    const encodeMax = Number(window.encodeUsMax ?? window.encodeUsP95);
    if (Number.isFinite(encodeMax) && encodeMax >= 0) total.encodeUsMax = Math.max(total.encodeUsMax, encodeMax);
    const encodeStages = window.encodeStages;
    if (encodeStages && typeof encodeStages === "object") {
      total.encodeStages ??= {
        frames: 0,
        register: { count: 0, totalUs: 0, maxUs: 0 },
        map: { count: 0, totalUs: 0, maxUs: 0 },
        submit: { count: 0, totalUs: 0, maxUs: 0 },
        lock: { count: 0, totalUs: 0, maxUs: 0 },
        copy: { count: 0, totalUs: 0, maxUs: 0 },
        unmap: { count: 0, totalUs: 0, maxUs: 0 },
        unregister: { count: 0, totalUs: 0, maxUs: 0 },
      };
      const frames = Number(encodeStages.frames);
      if (Number.isFinite(frames) && frames >= 0) total.encodeStages.frames += frames;
      for (const name of ["register", "map", "submit", "lock", "copy", "unmap", "unregister"]) {
        const stage = encodeStages[name];
        if (!stage || typeof stage !== "object") continue;
        for (const field of ["count", "totalUs"]) {
          const value = Number(stage[field]);
          if (Number.isFinite(value) && value >= 0) total.encodeStages[name][field] += value;
        }
        const maxUs = Number(stage.maxUs);
        if (Number.isFinite(maxUs) && maxUs >= 0)
          total.encodeStages[name].maxUs = Math.max(total.encodeStages[name].maxUs, maxUs);
      }
    }
    const activity = window.roiActivity;
    if (activity && typeof activity === "object") {
      total.roiActivity ??= {
        mode: activity.mode,
        evaluatedFrames: 0,
        mappedFrames: 0,
        missingDamageFrames: 0,
        emptyDamageFrames: 0,
        idrFrames: 0,
        coverageRejectedFrames: 0,
        unsupportedFrames: 0,
      };
      for (const field of ["evaluatedFrames", "mappedFrames", "missingDamageFrames", "emptyDamageFrames",
        "idrFrames", "coverageRejectedFrames", "unsupportedFrames"]) {
        const value = Number(activity[field]);
        if (Number.isFinite(value) && value >= 0) total.roiActivity[field] += value;
      }
      for (const field of ["temporalFrames", "temporalPrimingFrames", "feedbackSubmittedFrames",
        "feedbackReadyFrames", "feedbackLateFrames", "feedbackSkippedFrames",
        "feedbackAdjustedFrames", "feedbackWarmupFrames", "feedbackMotionBypasses",
        "feedbackMissingFrames", "feedbackEligibleCtbs", "feedbackRefinedCtbs",
        "feedbackFundedCtbs", "feedbackSpendHeadroomFrames", "feedbackNetZeroFrames"]) {
        const value = Number(activity[field]);
        if (Number.isFinite(value) && value >= 0)
          total.roiActivity[field] = (total.roiActivity[field] ?? 0) + value;
      }
      if (activity.source !== undefined) {
        total.roiActivity.source = total.roiActivity.source === undefined ||
          total.roiActivity.source === activity.source ? activity.source : "mixed";
      }
      for (const field of ["analysisP95Us", "analysisMaxUs", "feedbackSubmitP95Us",
        "feedbackSubmitMaxUs", "feedbackReadbackP95Us", "feedbackReadbackMaxUs",
        "feedbackWireOccupancyP95Bps", "feedbackWireOccupancyMaxBps"]) {
        const value = Number(activity[field]);
        if (Number.isFinite(value) && value >= 0)
          total.roiActivity[field] = Math.max(total.roiActivity[field] ?? 0, value);
      }
      if (activity.reconstructionActive !== undefined) {
        total.roiActivity.reconstructionActive = total.roiActivity.reconstructionActive === undefined
          ? activity.reconstructionActive === true
          : total.roiActivity.reconstructionActive && activity.reconstructionActive === true;
      }
      for (const field of ["lastSequence", "lastReason", "lastChangedCtbs", "lastTotalCtbs",
        "lastMinDelta", "lastMaxDelta", "lastFeedbackSequence", "lastFeedbackAgeFrames",
        "lastFeedbackBudgetPolicy", "lastFeedbackWireOccupancyBps"]) {
        if (activity[field] !== undefined) total.roiActivity[field] = activity[field];
      }
    }
    const hybrid = window.hybridActivity;
    if (hybrid && typeof hybrid === "object") {
      total.hybridActivity ??= { mode: hybrid.mode };
      for (const field of ["frames", "historyResets", "historyPreservedFrames", "motionBypasses",
        "cropRemaps", "cursorNeutralFrames", "candidates", "reissuedTiles", "upserts",
        "invalidations", "budgetSkippedUpserts", "pngBytes", "pngUsTotal", "sidecarBytes",
        "hevcTileBytes", "hevcTileCopyUsTotal", "hevcTileEncodeUsTotal",
        "hevcTileEncoderOpens", "hevcTileEncoderFailures",
        "validationFrames", "validationUnavailableFrames", "validationFailedFrames",
        "deferredValidationReadyFrames", "deferredValidationPendingFrames", "damageAuditFrames",
        "damageAuditMetadataUnavailableFrames", "damageAuditChangedCtbs", "damageAuditCoveredCtbs",
        "damageAuditMissedCtbs"]) {
        const value = Number(hybrid[field]);
        if (Number.isFinite(value) && value >= 0)
          total.hybridActivity[field] = (total.hybridActivity[field] ?? 0) + value;
      }
      for (const field of ["pngUsMax", "hevcTileCopyUsMax", "hevcTileEncodeUsMax",
        "analysisP95Us", "analysisMaxUs",
        "validationSubmitP95Us", "validationSubmitMaxUs", "validationCollectP95Us",
        "validationCollectMaxUs", "deferredValidationAgeP95Frames",
        "deferredValidationAgeMaxFrames", "deferredValidationPendingMaxAgeFrames"]) {
        const value = Number(hybrid[field]);
        if (Number.isFinite(value) && value >= 0)
          total.hybridActivity[field] = Math.max(total.hybridActivity[field] ?? 0, value);
      }
      for (const [compact, field] of Object.entries({
        s: "suppressions", r: "reactivations", i: "permanentInvalidations",
        x: "staleReactivationRejects", a: "pngBytesAvoided",
      })) {
        const value = Number(hybrid.cache?.[compact]);
        if (Number.isFinite(value) && value >= 0)
          total.hybridActivity[field] = (total.hybridActivity[field] ?? 0) + value;
      }
      const cachedBytes = Number(hybrid.cache?.b);
      if (Number.isFinite(cachedBytes) && cachedBytes >= 0)
        total.hybridActivity.cachedBytesRetained = Math.max(
          total.hybridActivity.cachedBytesRetained ?? 0, cachedBytes);
      if (hybrid.lastValidationSequence !== undefined)
        total.hybridActivity.lastValidationSequence = hybrid.lastValidationSequence;
    }
  }
  return total;
}

function summarizeBenchEncodeStages(value) {
  if (!value || typeof value !== "object") return null;
  const summary = { frames: value.frames };
  for (const name of ["register", "map", "submit", "lock", "copy", "unmap", "unregister"]) {
    const stage = value[name];
    const count = Number(stage?.count);
    const totalUs = Number(stage?.totalUs);
    const maxUs = Number(stage?.maxUs);
    summary[name] = {
      count,
      totalMs: Number.isFinite(totalUs) ? totalUs / 1_000 : null,
      meanMs: Number.isFinite(count) && count > 0 && Number.isFinite(totalUs)
        ? totalUs / count / 1_000 : null,
      maxMs: Number.isFinite(maxUs) ? maxUs / 1_000 : null,
    };
  }
  return summary;
}

function verifyBandwidth(actualMbps, capMbps, qosPolicy) {
  if (!Number.isFinite(capMbps)) return null;
  return qosPolicy?.active === true && qosPolicy.bandwidthMbps === capMbps &&
    Number.isFinite(actualMbps) && actualMbps <= capMbps * 1.05;
}

export function shapingFailureReasons(metrics, bandwidthMbps, qosPolicy) {
  if (!Number.isFinite(bandwidthMbps)) return [];
  const reasons = [];
  if (qosPolicy?.active !== true || qosPolicy.bandwidthMbps !== bandwidthMbps) reasons.push("qos-policy-unverified");
  if (!Number.isFinite(metrics?.deliveredWireBitrateMbps) || metrics.deliveredWireBitrateMbps > bandwidthMbps * 1.05)
    reasons.push("delivered-cap-overshoot");
  if (!Number.isFinite(metrics?.offeredWireBitrateMbps) || metrics.offeredWireBitrateMbps > bandwidthMbps * 0.95)
    reasons.push("offered-cap-overshoot");
  if ((metrics?.queueSlopePerMinute ?? 0) > 0) reasons.push("queue-growth");
  if (!Number.isFinite(metrics?.latencyP95Ms) || metrics.latencyP95Ms > 500) reasons.push("stream-latency");
  if ((metrics?.dependencyLosses ?? 0) > 0) reasons.push("dependency-loss");
  if ((metrics?.decoderResets ?? 0) > 0) reasons.push("decoder-reset");
  if ((metrics?.freezes ?? 0) > 0) reasons.push("freeze");
  return [...new Set(reasons)];
}

function benchMetrics(status) {
  if (status?.metrics && typeof status.metrics === "object") {
    const temporalRoi = status.metrics.temporalRoi;
    const roiActivity = status.roiActivity ?? status.metrics.roiActivity;
    return {
      ...status,
      ...status.metrics,
      ...(roiActivity ? {
        roiActivity: {
          ...roiActivity,
          ...(temporalRoi && typeof temporalRoi === "object" && !Array.isArray(temporalRoi)
            ? temporalRoi : {}),
        },
      } : {}),
      ...(status.encodeStages ? { encodeStages: status.encodeStages } : {}),
    };
  }
  return status ?? {};
}

function nonnegativeDelta(last, first) {
  const a = Number(last);
  const b = Number(first);
  return Number.isFinite(a) && Number.isFinite(b) ? Math.max(0, a - b) : 0;
}



function validLatency(value) {
  return Number.isFinite(value) && value >= 0 && value < 10_000;
}

export class CdpPage {
  constructor(port) { this.port = port; this.socket = null; }
  async open(timeoutMs = 15_000) {
    const target = await pageTarget(this.port, timeoutMs);
    this.socket = await cdpSocket(target.webSocketDebuggerUrl);
    await this.socket.call("Page.enable");
    await this.socket.call("Runtime.enable");
    return this;
  }
  call(method, params = {}) { return this.socket.call(method, params); }
  on(method, handler) { return this.socket.on(method, handler); }
  async evaluate(expression) {
    const response = await this.call("Runtime.evaluate", { expression, returnByValue: true, awaitPromise: true });
    if (response.exceptionDetails) throw new Error(response.exceptionDetails.exception?.description || response.exceptionDetails.text || "Runtime.evaluate failed");
    return response.result.value;
  }
  async navigate(url) { await this.call("Page.navigate", { url: String(url) }); }
  close() { this.socket?.close(); this.socket = null; }
}

async function cdpSocket(endpoint) {
  const ws = new WebSocket(endpoint);
  await new Promise((resolveOpen, reject) => { ws.onopen = resolveOpen; ws.onerror = () => reject(new Error("CDP WebSocket failed")); });
  let id = 0;
  const pending = new Map();
  const listeners = new Map();
  ws.onmessage = ({ data }) => {
    const message = JSON.parse(data);
    const resolver = pending.get(message.id);
    if (resolver) { pending.delete(message.id); resolver(message); return; }
    if (message.method) {
      for (const listener of listeners.get(message.method) ?? []) {
        Promise.resolve(listener(message.params ?? {})).catch(error => {
          process.stderr.write(`CDP event ${message.method} failed: ${error?.message || error}\n`);
        });
      }
    }
  };
  return {
    call(method, params = {}) {
      const requestId = ++id;
      ws.send(JSON.stringify({ id: requestId, method, params }));
      return new Promise((resolveCall, reject) => {
        pending.set(requestId, message => message.error ? reject(new Error(message.error.message)) : resolveCall(message.result));
      });
    },
    on(method, handler) {
      let handlers = listeners.get(method);
      if (!handlers) { handlers = new Set(); listeners.set(method, handlers); }
      handlers.add(handler);
      return () => {
        handlers.delete(handler);
        if (handlers.size === 0) listeners.delete(method);
      };
    },
    close: () => ws.close(),
  };
}

async function pageTarget(port, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try {
      const targets = await (await fetch(`http://127.0.0.1:${port}/json/list`)).json();
      const page = targets.find(target => target.type === "page");
      if (page?.webSocketDebuggerUrl) return page;
    } catch { /* Browser is still starting. */ }
    await sleep(100);
  }
  throw new StreamLabUnavailable("cdp-unavailable", `Chromium CDP endpoint on port ${port} did not become available.`);
}

export function enumerateDisplays() {
  const script = [
    "Add-Type -AssemblyName System.Windows.Forms",
    "[System.Windows.Forms.Screen]::AllScreens | ForEach-Object { [pscustomobject]@{ deviceName=$_.DeviceName; x=$_.Bounds.X; y=$_.Bounds.Y; width=$_.Bounds.Width; height=$_.Bounds.Height; primary=$_.Primary } } | ConvertTo-Json -Compress",
  ].join("; ");
  const result = spawnSync("powershell.exe", ["-NoProfile", "-NonInteractive", "-Command", script], { encoding: "utf8", windowsHide: true, timeout: 15_000 });
  if (result.status !== 0) throw new StreamLabUnavailable("display-enumeration", "Windows display enumeration failed.", result.stderr?.trim());
  const parsed = JSON.parse(result.stdout || "[]");
  return (Array.isArray(parsed) ? parsed : [parsed]).map(screen => ({
    deviceName: screen.deviceName, x: Number(screen.x), y: Number(screen.y),
    width: Number(screen.width), height: Number(screen.height), primary: Boolean(screen.primary),
  }));
}

export function enumerateStreamLabQosPolicies() {
  const script = [
    "$items=@(Get-NetQosPolicy -PolicyStore ActiveStore -ErrorAction Stop | Where-Object {$_.Name -like 'BubblePlay-StreamLab-*' -or $_.Name -like 'BubblePlayLab-*'} | Select-Object -ExpandProperty Name)",
    "ConvertTo-Json -Compress -InputObject $items",
  ].join("; ");
  const result = spawnSync("powershell.exe", ["-NoProfile", "-NonInteractive", "-Command", script],
    { encoding: "utf8", windowsHide: true, timeout: 15_000 });
  if (result.status !== 0) throw new StreamLabUnavailable("qos-enumeration", "Windows ActiveStore QoS policy enumeration failed.", result.stderr?.trim());
  const parsed = JSON.parse(result.stdout || "[]");
  return Array.isArray(parsed) ? parsed.map(String) : parsed ? [String(parsed)] : [];
}

export function inspectRunQosPolicy(runId) {
  if (!/^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(String(runId)))
    throw new RangeError("runId must be a canonical UUID");
  const name = `BubblePlayLab-${String(runId).replaceAll("-", "")}`;
  const escapedName = name.replaceAll("'", "''");
  const script = [
    `$p=Get-NetQosPolicy -Name '${escapedName}' -PolicyStore ActiveStore -ErrorAction SilentlyContinue`,
    "if($null -eq $p){'null'}else{[pscustomobject]@{name=$p.Name;bitsPerSecond=[uint64]$p.ThrottleRateActionBitsPerSecond}|ConvertTo-Json -Compress}",
  ].join("; ");
  const result = spawnSync("powershell.exe", ["-NoProfile", "-NonInteractive", "-Command", script],
    { encoding: "utf8", windowsHide: true, timeout: 15_000 });
  if (result.status !== 0) throw new StreamLabUnavailable("qos-inspection",
    `Windows could not inspect the run-owned QoS policy ${name}.`, result.stderr?.trim());
  const parsed = JSON.parse(String(result.stdout || "null").trim() || "null");
  if (!parsed) return { active: false, name, bandwidthMbps: null, bitsPerSecond: null };
  const bitsPerSecond = Number(parsed.bitsPerSecond);
  return { active: true, name: String(parsed.name), bitsPerSecond,
    bandwidthMbps: Number.isFinite(bitsPerSecond) ? bitsPerSecond / 1_000_000 : null };
}

export function inspectCleanupResources(options = {}) {
  const evidenceAvailability = { service: false, displays: false, qos: false, chrome: false };
  let service = options.service ?? null;
  if (!service && !Object.hasOwn(options, "service")) {
    try { service = (options.serviceProbe ?? inspectLabService)(); } catch { service = null; }
  }
  evidenceAvailability.service = Boolean(service?.installed && service?.queryable && Number.isInteger(service?.state));

  let currentDisplays = options.currentDisplays ?? null;
  if (!currentDisplays && !Object.hasOwn(options, "currentDisplays")) {
    try { currentDisplays = (options.displayProbe ?? enumerateDisplays)(); } catch { currentDisplays = null; }
  }
  evidenceAvailability.displays = Array.isArray(options.preDisplays) && Array.isArray(currentDisplays) &&
    Boolean(options.transientDisplay?.deviceName);
  const transientName = String(options.transientDisplay?.deviceName || "").toLowerCase();
  const transientVddAbsent = evidenceAvailability.displays &&
    !currentDisplays.some(display => String(display.deviceName).toLowerCase() === transientName);
  const persistentDisplay = options.persistentDisplay === true;
  const persistentVddPresent = evidenceAvailability.displays &&
    currentDisplays.some(display => String(display.deviceName).toLowerCase() === transientName);
  const displayResourcesClean = persistentDisplay ? persistentVddPresent : transientVddAbsent;

  let qosPolicies = options.qosPolicies ?? null;
  if (!qosPolicies && !Object.hasOwn(options, "qosPolicies")) {
    try { qosPolicies = (options.qosProbe ?? enumerateStreamLabQosPolicies)(); } catch { qosPolicies = null; }
  }
  evidenceAvailability.qos = Array.isArray(qosPolicies);

  const childProcesses = Array.isArray(options.childProcesses) ? options.childProcesses.map(child => ({
    role: child.role, pid: child.pid,
    alive: typeof child.alive === "boolean" ? child.alive : processIsAlive(child.pid),
  })) : [];
  evidenceAvailability.chrome = childProcesses.length >= 2 &&
    childProcesses.every(child => Number.isInteger(child.pid) && child.pid > 0 && typeof child.alive === "boolean");
  return {
    evidenceAvailability,
    service,
    serviceStopped: evidenceAvailability.service && service.state === SERVICE_STATE.STOPPED,
    preDisplays: options.preDisplays ?? null,
    currentDisplays,
    transientDisplay: options.transientDisplay ?? null,
    transientVddAbsent,
    persistentDisplay,
    persistentVddPresent,
    displayResourcesClean,
    activeQosPolicies: qosPolicies,
    qosPoliciesAbsent: evidenceAvailability.qos && qosPolicies.length === 0,
    chromeChildren: childProcesses,
    chromeChildrenExited: evidenceAvailability.chrome && childProcesses.every(child => !child.alive),
  };
}

function processIsAlive(pid) {
  if (!Number.isInteger(pid) || pid <= 0) return null;
  const script = `$p=Get-Process -Id ${pid} -ErrorAction SilentlyContinue; if($p){exit 0}else{exit 1}`;
  const result = spawnSync("powershell.exe", ["-NoProfile", "-NonInteractive", "-Command", script],
    { encoding: "utf8", windowsHide: true, timeout: 5_000 });
  return result.status === 0;
}

function launchChrome(chrome, args, options = {}) {
  return spawn(chrome, args, { stdio: "ignore", windowsHide: options.windowsHide ?? true });
}

function terminateTree(child) {
  if (!child?.pid) return;
  if (process.platform === "win32") spawnSync("taskkill", ["/PID", String(child.pid), "/T", "/F"], { stdio: "ignore", windowsHide: true });
  else child.kill("SIGTERM");
}

export async function shutdownChrome(page, child, options = {}) {
  if (!child?.pid) return "absent";
  const closeTimeoutMs = options.closeTimeoutMs ?? 1_000;
  const exitTimeoutMs = options.exitTimeoutMs ?? 3_000;
  const close = Promise.resolve()
    .then(() => page?.call("Browser.close"))
    .catch(() => undefined);
  await Promise.race([close, sleep(closeTimeoutMs)]);
  if (await waitForChildExit(child, exitTimeoutMs)) return "graceful";
  (options.forceKill ?? (() => terminateTree(child)))(child.pid);
  return "forced";
}

async function shutdownChild(child, options = {}) {
  if (!child?.pid) return "absent";
  try { child.kill(); } catch {}
  if (await waitForChildExit(child, options.exitTimeoutMs ?? 3_000)) return "graceful";
  terminateTree(child);
  return "forced";
}

function waitForChildExit(child, timeoutMs) {
  if (child.exitCode !== null && child.exitCode !== undefined) return Promise.resolve(true);
  return new Promise(resolveExit => {
    const onExit = () => {
      clearTimeout(timer);
      resolveExit(true);
    };
    const timer = setTimeout(() => {
      child.removeListener?.("exit", onExit);
      resolveExit(false);
    }, timeoutMs);
    child.once("exit", onExit);
  });
}

async function waitForNewDisplay(before, timeoutMs = 15_000) {
  const deadline = Date.now() + timeoutMs;
  let last = before;
  while (Date.now() < deadline) {
    last = enumerateDisplays();
    try { return selectTransientDisplay(before, last); } catch (error) {
      if (error.code !== "vdd-not-created") throw error;
    }
    await sleep(250);
  }
  return selectTransientDisplay(before, last);
}

export class LiveStreamLabSession {
  constructor(options) {
    this.runId = options.runId;
    this.store = options.store;
    this.env = options.env ?? process.env;
    this.baseUrl = options.baseUrl || this.env.BUBBLEPLAY_URL || DEFAULT_URL;
    this.chrome = options.chrome || findChrome(this.env);
    this.workerVersionOverride = options.workerVersionOverride ?? workerVersionOverrideFromEnv(this.env);
    this.receiverAssetOverride = options.receiverAssetOverride ??
      (Boolean(this.workerVersionOverride) ||
        ["1", "true", "local"].includes(
          String(this.env.BUBBLEPLAY_RECEIVER_ASSET_OVERRIDE || "").trim().toLowerCase(),
        ));
    this.expectedReceiverAssetBuild = options.expectedReceiverAssetBuild ??
      (String(this.env.BUBBLEPLAY_EXPECTED_ASSET_BUILD || "").trim() ||
        (this.workerVersionOverride || this.receiverAssetOverride ? STREAM_LAB_ASSET_BUILD : null));
    this.resetEncoderOnQualityKeyframe = options.resetEncoderOnQualityKeyframe ??
      !["0", "false", "off"].includes(
        String(this.env.BUBBLEPLAY_LAB_RESET_ENCODER_ON_DIAGNOSTIC_KEYFRAME || "").trim().toLowerCase(),
      );
    this.authTimeoutMs = options.authTimeoutMs ?? 120_000;
    this.receiverPort = options.receiverPort ?? 19341;
    this.sourcePort = options.sourcePort ?? 19342;
    this.receiver = null;
    this.source = null;
    this.receiverWebSocketDiagnosticsDispose = [];
    this.receiverProcess = null;
    this.sourceProcess = null;
    this.candidateProcess = null;
    this.display = null;
    this.runScratch = join(resolve(persistentReceiverProfile(this.env), "..", "runs"), this.runId);
    this.candidateHost = options.candidateHost ?? resolve(import.meta.dirname, "..", "..", "host", "target-candidate", "release", "bubbleplay-host.exe");
    this.candidateProfilePath = join(this.runScratch, "candidate-profile.json");
    this.candidateProfileLease = null;
    this.candidateProfileRevision = 0;
    this.firstFrameStartedAt = null;
    this.firstFrameMs = null;
    this.lastFirstFrameMs = null;
    this.applyExpectedSource = null;
    this.transport = "wss";
    this.receiverOptions = { relay: true, adaptive: false, decoderPath: "webcodecs" };
    this.heartbeat = null;
    this.disposeTermination = null;
    this.stopped = false;
    this.stopPromise = null;
    this.preRunDisplays = null;
    this.preCampaignSystem = null;
    this.cleanupEvidence = null;
    this.hybridWorkloads = [];
    this.monitorSelectionRegisteredAtMs = null;
    this.assertCampaignActive = options.assertCampaignActive ?? (() => {});
    this.helperSessionFactory = options.helperSessionFactory ?? openDemandStartedHelperSession;
    this.helperSession = options.helperSession ?? null;
    this.ownsHelperSession = !options.helperSession;
    this.transientVdd = options.transientVdd === true;
    // ActiveStore policies installed by LocalSystem are not necessarily
    // enumerable in the unelevated user's policy view. The helper's bounded
    // command acknowledgement is authoritative; an injected probe can add a
    // second assertion in tests or environments where that store is visible.
    this.qosPolicyProbe = options.qosPolicyProbe ?? null;
    this.activeBandwidthMbps = undefined;
    this.qosPolicy = { active: false, bandwidthMbps: null, name: null };
    this.stabilizedTargets = new Map();
  }

  async start(transport = "wss", options = {}) {
    const startup = sessionStartupPlan(options.mode ?? "benchmark");
    this.disposeTermination ??= installTerminationCleanup(async signal => {
      try { await this.stop({ fromSignal: true }); }
      finally {
        try {
          const state = this.store.readCampaignState?.();
          if (state && ["running", "paused"].includes(state.status))
            this.store.writeCampaignState?.(interruptedCampaignState(state, signal));
        } catch { /* A non-campaign run has no campaign state to update. */ }
      }
    });
    if (!this.chrome) throw new StreamLabUnavailable("chromium-missing", "Chrome or Edge was not found.");
    this.transport = transport;
    mkdirSync(this.runScratch, { recursive: true });
    writeFileSync(join(this.runScratch, "lease.json"), `${JSON.stringify({ runId: this.runId, expiresAt: new Date(Date.now() + 600_000).toISOString() })}\n`);
    const displaysBefore = enumerateDisplays();
    this.preRunDisplays = displaysBefore;
    if (!this.helperSession) this.helperSession = await this.helperSessionFactory({
      runId: this.runId, leaseSeconds: 600, vdd: this.transientVdd,
    });
    this.activeBandwidthMbps = null;
    this.qosPolicy = { active: false, bandwidthMbps: null, name: null, evidence: "helper-begin" };
    this.preCampaignSystem = sampleSystemTelemetry();
    process.stderr.write("Stream Lab: opening the persistent receiver browser.\n");
    await this.startReceiver(transport, this.receiverOptions, {
      startMinimized: startup.receiverStartMinimized,
    });
    await this.admitCompatibility();
    process.stderr.write(`Stream Lab: starting the isolated uninstalled candidate host on the ${this.transientVdd ? "transient" : "persistent"} VDD.\n`);
    await this.startCandidateHost();
    this.heartbeat = new SerializedLeaseHeartbeat(async () => {
      writeFileSync(join(this.runScratch, "lease.json"), `${JSON.stringify({ runId: this.runId, expiresAt: new Date(Date.now() + 600_000).toISOString() })}\n`);
      if (this.candidateProfileLease) {
        this.candidateProfileLease = { ...this.candidateProfileLease, expiresAtMs: Date.now() + 600_000 };
        publishCandidateProfile(this.candidateProfilePath, this.candidateProfileLease);
      }
      await this.helperSession.renew(this.runId, 600);
      this.store.appendTelemetry({ phase: "candidate-lease-renewal", at: new Date().toISOString(), result: "renewed" });
    });
    this.heartbeat.start();
    process.stderr.write(`Stream Lab: locating the ${this.transientVdd ? "Lab Helper-owned transient" : "Broker-owned persistent"} virtual display.\n`);
    this.display = this.transientVdd
      ? await waitForNewDisplay(displaysBefore)
      : await this.resolvePersistentVirtualDisplay(displaysBefore);
    if (startup.placeReceiverOnCaptureDisplay) await this.placeReceiverOnLabDisplay();
    if (startup.startSyntheticSource) {
      process.stderr.write(`Stream Lab: ${this.transientVdd ? "transient" : "persistent"} display ready (${this.display.deviceName}); opening the synthetic source.\n`);
      await this.startSource();
      process.stderr.write("Stream Lab: source, receiver, and candidate host are ready.\n");
    } else {
      process.stderr.write(`Stream Lab: manual receiver and candidate host are ready on ${this.display.deviceName}; no interaction automation was started.\n`);
    }
    return this;
  }

  async startReceiver(transport, receiverOptions = this.receiverOptions, launchOptions = {}) {
    this.receiverOptions = normalizeReceiverOptions(receiverOptions, this.receiverOptions);
    const url = buildReceiverUrl(this.baseUrl, transport, { ...this.receiverOptions, labRun: this.runId });
    const bootstrapUrl = receiverBootstrapUrl(url.href, {
      workerVersionOverride: this.workerVersionOverride,
      receiverAssetOverride: this.receiverAssetOverride,
    });
    this.receiverProcess = launchChrome(this.chrome,
      buildReceiverChromeArgs(persistentReceiverProfile(this.env), this.receiverPort, bootstrapUrl, launchOptions));
    try { this.receiver = await new CdpPage(this.receiverPort).open(); }
    catch (error) { throw new StreamLabUnavailable("receiver-profile-in-use", "The persistent Stream Lab Chrome profile could not open. Close another Stream Lab browser and retry.", String(error)); }
    this.receiverWebSocketDiagnosticsDispose = installReceiverWebSocketDiagnostics(
      this.receiver,
      this.store,
      this.workerVersionOverride,
    );
    await this.receiver.call("Emulation.setDeviceMetricsOverride", { width: 1920, height: 1080, deviceScaleFactor: 1, mobile: false });
    let navigate = bootstrapUrl !== url.href;
    navigate = await applyWorkerVersionOverride(this.receiver, this.workerVersionOverride) || navigate;
    if (this.receiverAssetOverride) {
      await installReceiverAssetOverrides(this.receiver, { baseUrl: this.baseUrl });
      navigate = true;
    }
    if (navigate)
      await this.receiver.navigate(url.href);
    await this.waitForAuthentication();
  }

  async admitCompatibility() {
    process.stderr.write("Stream Lab: verifying the pinned Worker compatibility contract.\n");
    await this.verifyWorkerCompatibility();
    process.stderr.write("Stream Lab: verifying the local receiver compatibility contract.\n");
    await this.verifyReceiverCompatibility();
  }

  async verifyReceiverCompatibility() {
    if (this.expectedReceiverAssetBuild) {
      let last = null;
      const matched = await poll(async () => {
        last = await this.snapshot();
        try { return assertReceiverCompatibility(last); }
        catch { return false; }
      }, 5_000);
      if (!matched) assertReceiverCompatibility(last);
      this.store.appendTelemetry({ phase: "receiver-asset-build", at: new Date().toISOString(),
        expected: this.expectedReceiverAssetBuild, actual: matched });
    }
  }

  async verifyWorkerCompatibility() {
    const result = await this.receiver.evaluate(`(async () => {
      try {
        const response = await fetch("/lab-compat", { cache: "no-store" });
        if (!response.ok) return { ok: false, status: response.status, compatibility: null };
        return await response.json();
      } catch (error) {
        return { ok: false, status: null, compatibility: null, error: String(error) };
      }
    })()`);
    const compatibility = assertWorkerCompatibility(result);
    this.store.appendTelemetry({
      phase: "worker-compatibility",
      at: new Date().toISOString(),
      workerVersionOverride: this.workerVersionOverride,
      compatibility,
    });
    return compatibility;
  }

  async resolvePersistentVirtualDisplay(displays) {
    const deadline = Date.now() + 15_000;
    let lastMonitors = [];
    let refreshedAt = 0;
    while (Date.now() < deadline) {
      const snapshot = await this.snapshot();
      lastMonitors = snapshot?.signaling?.monitors ?? [];
      try { return selectPersistentVirtualDisplay(enumerateDisplays(), lastMonitors); }
      catch (error) {
        if (!["persistent-vdd-unavailable", "persistent-vdd-screen-missing"].includes(error?.code)) throw error;
      }
      if (snapshot?.signaling?.registered && Date.now() - refreshedAt >= 1_000) {
        await this.receiver.evaluate("window.__bubbleplayDiagnostics?.requestMonitors?.() ?? false");
        refreshedAt = Date.now();
      }
      await sleep(250);
    }
    return selectPersistentVirtualDisplay(displays, lastMonitors);
  }

  async placeReceiverOnLabDisplay() {
    const target = await this.receiver.call("Browser.getWindowForTarget");
    await this.receiver.call("Browser.setWindowBounds", { windowId: target.windowId, bounds: { windowState: "normal" } });
    await this.receiver.call("Browser.setWindowBounds", { windowId: target.windowId,
      bounds: { left: this.display.x, top: this.display.y, width: this.display.width, height: this.display.height } });
  }

  async waitForAuthentication() {
    const deadline = Date.now() + this.authTimeoutMs;
    let sawLogin = false;
    let lastState = null;
    while (Date.now() < deadline) {
      const state = await this.receiver.evaluate(`(() => ({
        app: !!document.querySelector("#app-brand"),
        diagnostics: typeof window.__bubbleplayDiagnostics?.snapshot === "function",
        login: !!document.querySelector("#login"),
        labRun: new URLSearchParams(location.search).get("labRun"),
        title: document.title,
        text: document.body?.innerText?.slice(0,160) || "",
      }))()`);
      lastState = state;
      if (receiverAuthenticationReady(state, this.runId)) return;
      if (state?.login && !sawLogin) {
        sawLogin = true;
        process.stderr.write("Stream Lab authentication is required in the opened Chrome window; approve and log in once to remember this profile.\n");
      }
      await sleep(250);
    }
    throw new StreamLabUnavailable(sawLogin ? "authentication-required" : "approval-timeout",
      sawLogin ? "The persistent receiver profile was not logged in before the timeout." : "The server-held browser approval did not complete before the timeout.",
      { stage: "authentication", browser: lastState });
  }

  async startSource() {
    const sourceProfile = join(this.runScratch, "source-chrome");
    mkdirSync(sourceProfile, { recursive: true });
    const sourceUrl = pathToFileURL(resolve(import.meta.dirname, "..", "public", "stream-lab-source.html"));
    sourceUrl.searchParams.set("scene", "ui-text"); sourceUrl.searchParams.set("seed", "469");
    this.sourceProcess = launchChrome(this.chrome, [
      `--remote-debugging-port=${this.sourcePort}`, `--user-data-dir=${sourceProfile}`,
      "--no-first-run", "--disable-background-timer-throttling", "--disable-backgrounding-occluded-windows",
      "--disable-renderer-backgrounding", "--force-device-scale-factor=1",
      `--window-position=${this.display.x},${this.display.y}`, `--window-size=${this.display.width},${this.display.height}`,
      `--app=${sourceUrl.href}`,
    ], { windowsHide: false });
    this.source = await new CdpPage(this.sourcePort).open();
    const sourceWindow = await placeSourceWindow(this.source, this.sourceProcess.pid, this.display);
    this.store.appendTelemetry({ phase: "source-window-placement", at: new Date().toISOString(),
      display: this.display, bounds: sourceWindow?.bounds ?? null });
    if (!windowCoversDisplay(sourceWindow?.bounds, this.display))
      throw new StreamLabUnavailable("source-window-placement", "The deterministic source window did not cover the transient display.",
        { display: this.display, bounds: sourceWindow?.bounds ?? null });
    await this.source.call("Emulation.setDeviceMetricsOverride", { width: 1920, height: 1080, deviceScaleFactor: 1, mobile: false });
    const metadata = await this.source.evaluate("window.__bubbleplaySource?.metadata?.() ?? null");
    if (metadata?.width !== 1920 || metadata?.height !== 1080)
      throw new StreamLabUnavailable("source-not-ready", "The deterministic source kiosk did not initialize at 1920×1080.", metadata);
    this.hybridWorkloads = buildHybridWorkloadPlan();
    const sourceWorkload = this.hybridWorkloads.find(workload => workload.id === "source-code-fixture");
    const allowlistedPath = sourceWorkload?.allowlistedFiles?.[0];
    if (allowlistedPath) {
      const absolutePath = resolve(import.meta.dirname, "..", "..", allowlistedPath);
      const fixture = readFileSync(absolutePath, "utf8").slice(0, 65_536);
      await this.source.evaluate(`window.__bubbleplaySource.setSourceCode(${JSON.stringify(fixture)})`);
    }
  }

  async startCandidateHost() {
    if (!existsSync(this.candidateHost))
      throw new StreamLabUnavailable("candidate-host-missing", `Candidate host is not built at ${this.candidateHost}.`);
    const launch = buildCandidateHostLaunch(
      this.candidateHost,
      this.baseUrl,
      this.runId,
      this.candidateProfilePath,
      {
        resetEncoderOnQualityKeyframe: this.resetEncoderOnQualityKeyframe,
        workerVersionOverride: this.workerVersionOverride,
      },
    );
    const stdoutPath = join(this.store.path, "candidate-host.stdout.log");
    const stderrPath = join(this.store.path, "candidate-host.stderr.log");
    const stdout = openSync(stdoutPath, "a");
    const stderr = openSync(stderrPath, "a");
    try {
      this.candidateProcess = spawn(launch.command, launch.args, {
        cwd: resolve(import.meta.dirname, "..", "..", "host"),
        env: { ...this.env, ...launch.env, RUST_LOG: this.env.BUBBLEPLAY_LAB_RUST_LOG || "info" },
        stdio: ["ignore", stdout, stderr], windowsHide: true,
      });
    } finally {
      closeSync(stdout); closeSync(stderr);
    }
    this.candidateProcess.once("exit", (code, signal) => {
      this.store.appendTelemetry({ phase: "candidate-host-exit", at: new Date().toISOString(), code, signal });
    });
    await sleep(250);
    if (this.candidateProcess.exitCode !== null)
      throw new StreamLabUnavailable("candidate-host-exited", `Candidate host exited with code ${this.candidateProcess.exitCode}.`, { stderrPath });
  }

  async applyCandidateProfile(candidate, bandwidthMbps = null, transport = this.transport) {
    const decoderPath = normalizeDecoderPath(this.receiverOptions?.decoderPath);
    if (transport === "webrtc" && decoderPath === "native")
      throw new StreamLabUnavailable("native-benchmark-unavailable",
        "Native playback has no source-barcode measurement; use the separate native HEVC acceptance check.");
    if (!decoderSupportsCodec(transport, decoderPath, candidate.codec ?? "hevc"))
      throw new StreamLabUnavailable("unsupported-decoder-codec",
        "The diagnostic WebCodecs decoder supports H264 only; native HEVC requires the native decoder path.",
        { transport, decoderPath, codec: candidate.codec ?? "hevc" });
    this.assertLeaseHealthy();
    await this.setBandwidthCap(bandwidthMbps);
    const profile = benchmarkProfile(candidate, transport);
    const preApplyBenchStatus = (await this.snapshot())?.benchStatus ?? null;
    this.candidateProfileRevision++;
    this.candidateProfileLease = buildCandidateProfileLease(profile, this.runId, Date.now(), this.candidateProfileRevision);
    publishCandidateProfile(this.candidateProfilePath, this.candidateProfileLease);
    await this.receiver.evaluate(`window.__bubbleplayDiagnostics?.setStreamMode?.(${JSON.stringify(profile.streamMode)}) ?? false`);
    await this.selectVirtualMonitor();
    const profileApplied = await poll(async () => {
      this.assertLeaseHealthy();
      const snapshot = await this.snapshot();
      return benchStatusConfirmsApply(preApplyBenchStatus, snapshot?.benchStatus, profile, this.runId) ? snapshot : false;
    }, 10_000);
    if (!profileApplied) {
      const diagnostics = await this.snapshot();
      const code = classifyCandidateProfileFailure(diagnostics);
      const messages = {
        "bench-status-rejected": "The pinned Worker rejected the candidate benchmark status.",
        "media-packet-rejected": "The receiver rejected one or more WSS media packets before profile confirmation.",
        "wss-startup-no-reconstruction": "WSS packets arrived but no frame reconstructed before profile confirmation.",
        "candidate-profile-timeout":
          "The isolated candidate did not confirm its local benchmark profile before source activation.",
      };
      throw new StreamLabUnavailable(code, messages[code], { diagnostics });
    }
    return { profile, preApplyBenchStatus, profileApplied };
  }

  async applyForManualUse(candidate, bandwidthMbps = null, transport = this.transport, readiness = {}) {
    const applied = await this.applyCandidateProfile(candidate, bandwidthMbps, transport);
    const snapshot = await this.waitForMediaPrimed(candidate, transport, {
      ...readiness,
      preApplyBenchStatus: applied.preApplyBenchStatus,
    });
    this.store.appendTelemetry({
      phase: "manual-use-ready",
      at: new Date().toISOString(),
      experimentId: applied.profile.experimentId ?? null,
      hybridMode: applied.profile.hybridMode ?? "off",
      route: snapshot?.route ?? null,
      decoder: snapshot?.dec ?? null,
      presented: snapshot?.presented ?? snapshot?.wssMedia?.presented ?? null,
      interactionAutomation: false,
    });
    return snapshot;
  }

  async apply(candidate, bandwidthMbps = null, transport = this.transport, readiness = {}) {
    const applied = await this.applyCandidateProfile(candidate, bandwidthMbps, transport);
    await this.waitForMediaPrimed(candidate, transport, {
      ...readiness,
      preApplyBenchStatus: applied.preApplyBenchStatus,
    });
    this.firstFrameStartedAt = performance.now();
    this.applyExpectedSource = await this.source.evaluate("window.__bubbleplaySource.triggerAction()");
    await this.waitForReady(candidate, transport, {
      ...readiness,
      preApplyBenchStatus: applied.preApplyBenchStatus,
    });
  }

  async waitForQosPolicy(bandwidthMbps, timeoutMs = 10_000) {
    let last = null;
    const expected = bandwidthMbps === null ? null : Number(bandwidthMbps);
    const matched = await poll(async () => {
      last = await this.qosPolicyProbe?.(expected);
      if (expected === null) return last?.active === false ? last : false;
      return last?.active === true && Number.isFinite(last.bandwidthMbps) &&
        Math.abs(last.bandwidthMbps - expected) <= Math.max(0.01, expected * 0.001) ? last : false;
    }, timeoutMs, 100);
    if (!matched) throw new StreamLabUnavailable("bandwidth-shaping-unverified",
      "The run-owned Windows QoS policy did not match the requested bandwidth cap.",
      { requestedBandwidthMbps: bandwidthMbps, policy: last });
    return matched;
  }

  async setBandwidthCap(bandwidthMbps) {
    const requested = bandwidthMbps === null || bandwidthMbps === undefined ? null : Number(bandwidthMbps);
    if (requested !== null && (!Number.isFinite(requested) || requested <= 0))
      throw new RangeError("bandwidthMbps must be null or positive");
    if (this.activeBandwidthMbps === requested) return this.qosPolicy;
    if (!this.helperSession?.shape)
      throw new StreamLabUnavailable("bandwidth-shaping-unavailable", "The privileged Stream Lab helper session is unavailable.");
    const helperEvidence = await this.helperSession.shape(this.runId, requested, 600);
    if (!helperEvidence || helperEvidence.ok !== true || helperEvidence.event !== "shaped")
      throw new StreamLabUnavailable("bandwidth-shaping-unverified",
        "The privileged helper did not acknowledge the requested QoS transition.",
        { requestedBandwidthMbps: requested, helperEvidence: helperEvidence ?? null });
    this.qosPolicy = this.qosPolicyProbe ? await this.waitForQosPolicy(requested) : {
      active: requested !== null,
      bandwidthMbps: requested,
      name: `BubblePlayLab-${this.runId.replaceAll("-", "")}`,
      evidence: "helper-shape-ack",
      helperEvent: helperEvidence.event,
      expiresAtUnixMs: helperEvidence.expiresAtUnixMs ?? null,
    };
    this.activeBandwidthMbps = requested;
    this.store.appendTelemetry({ phase: "bandwidth-shape", at: new Date().toISOString(),
      requestedBandwidthMbps: requested, policy: this.qosPolicy });
    if (this.transport === "wss") await this.restartTransport("wss", this.receiverOptions);
    return this.qosPolicy;
  }

  async switchTransport(transport, receiverOptions = {}) {
    this.assertLeaseHealthy();
    const next = normalizeReceiverOptions(receiverOptions, this.receiverOptions);
    if (transport === this.transport && sameReceiverOptions(next, this.receiverOptions)) return;
    this.transport = transport;
    this.receiverOptions = next;
    await this.receiver.navigate(buildReceiverUrl(this.baseUrl, transport, { ...next, labRun: this.runId }).href);
    await this.waitForAuthentication();
  }

  async restartTransport(transport = this.transport, receiverOptions = this.receiverOptions) {
    this.assertLeaseHealthy();
    this.transport = transport;
    this.receiverOptions = normalizeReceiverOptions(receiverOptions, this.receiverOptions);
    const url = buildReceiverUrl(this.baseUrl, transport, { ...this.receiverOptions, labRun: this.runId });
    url.searchParams.set("labAttempt", String(Date.now()));
    await this.receiver.navigate(url.href);
    await this.waitForAuthentication();
  }

  async recoverAfterRejectedCandidate(safeConfiguration) {
    this.assertLeaseHealthy();
    this.store.appendTelemetry({ phase: "encoder-candidate-recovery-start", at: new Date().toISOString() });
    await shutdownChild(this.candidateProcess);
    this.candidateProcess = null;
    this.candidateProfileLease = null;
    rmSync(this.candidateProfilePath, { force: true });
    await this.restartTransport("wss", this.receiverOptions);
    await this.startCandidateHost();
    await this.apply(safeConfiguration, null, "wss");
    this.store.appendTelemetry({ phase: "encoder-candidate-recovery-complete", at: new Date().toISOString(),
      experimentId: safeConfiguration.experimentId });
  }

  async selectVirtualMonitor() {
    this.assertLeaseHealthy();
    const deadline = Date.now() + 15_000;
    const targetSelector = String(this.display?.deviceName || "");
    const normalizedTarget = targetSelector.toLowerCase();
    if (!targetSelector || this.display?.primary === true)
      throw new StreamLabUnavailable("persistent-vdd-geometry",
        "The Stream Lab monitor target must be the exact non-primary persistent virtual display.", this.display);
    let snapshot = await this.snapshot();
    let refreshBaseRevision = Number(snapshot?.signaling?.monitorRevision) || 0;
    let refreshRequested = false;
    let selectionRevision = null;
    let selectionRequestedAt = 0;
    let selectionAttempts = 0;
    let sawRegistered = false;
    let sawFreshMonitorList = false;
    let sawExactTarget = false;
    let sawHostRejection = false;
    let sawDeferredSelection = false;
    const selectionStartedAt = Date.now();
    while (Date.now() < deadline) {
      this.assertLeaseHealthy();
      snapshot = await this.snapshot();
      const signaling = snapshot?.signaling;
      const revision = Number(signaling?.monitorRevision) || 0;
      if (signaling?.registered !== true) {
        refreshRequested = false;
        selectionRevision = null;
        refreshBaseRevision = revision;
        await sleep(250);
        continue;
      }
      sawRegistered = true;
      if (selectionRevision !== null) {
        if (revision > selectionRevision) {
          const exact = (signaling.monitors ?? []).filter(monitor =>
            String(monitor?.selector || "").toLowerCase() === normalizedTarget);
          if (exact.length === 1 && exact[0].selected === true) {
            this.monitorSelectionRegisteredAtMs = Number(signaling.registeredAtMs);
            return snapshot;
          }
          refreshRequested = false;
          selectionRevision = null;
          refreshBaseRevision = revision;
        } else if (Date.now() - selectionRequestedAt >= 1_000) {
          refreshRequested = false;
          selectionRevision = null;
          refreshBaseRevision = revision;
        }
        await sleep(250);
        continue;
      }
      if (!refreshRequested) {
        const accepted = await this.receiver.evaluate("window.__bubbleplayDiagnostics?.requestMonitors?.() ?? false");
        if (accepted) {
          refreshRequested = true;
          refreshBaseRevision = revision;
        }
        await sleep(250);
        continue;
      }
      if (revision <= refreshBaseRevision) {
        await sleep(250);
        continue;
      }
      sawFreshMonitorList = true;
      const exact = (signaling.monitors ?? []).filter(monitor =>
        String(monitor?.selector || "").toLowerCase() === normalizedTarget);
      if (exact.length !== 1) {
        refreshRequested = false;
        refreshBaseRevision = revision;
        await sleep(250);
        continue;
      }
      sawExactTarget = true;
      selectionAttempts++;
      const accepted = await this.receiver.evaluate(
        `window.__bubbleplayDiagnostics?.selectMonitor?.(${JSON.stringify(exact[0].selector)}) ?? false`);
      if (accepted) {
        selectionRevision = revision;
        selectionRequestedAt = Date.now();
      } else {
        // Distinguish "the receiver could not issue the control yet" from
        // "the host rejected this exact selector": only the latter is a
        // monitor-selection rejection.
        const hostError = signaling?.lastMonitorError;
        const rejectedByHost = hostError &&
          String(hostError.selector || "").toLowerCase() === normalizedTarget &&
          Number(hostError.atMs) >= selectionStartedAt;
        if (rejectedByHost) sawHostRejection = true;
        else sawDeferredSelection = true;
        refreshRequested = false;
        refreshBaseRevision = revision;
      }
      await sleep(250);
    }
    const detail = { targetSelector, selectionAttempts, diagnostics: snapshot };
    const failure = classifyMonitorSelectionFailure({
      sawRegistered, sawFreshMonitorList, sawExactTarget, sawHostRejection, sawDeferredSelection,
    });
    throw new StreamLabUnavailable(failure.code, failure.message, detail);
  }

  async ensureVirtualMonitorSelected(snapshot = null) {
    const current = snapshot ?? await this.snapshot();
    if (virtualMonitorSelectionCurrent(current, this.display, this.monitorSelectionRegisteredAtMs))
      return { snapshot: current, reselected: false };
    return { snapshot: await this.selectVirtualMonitor(), reselected: true };
  }

  async waitForReady(candidate, transport, readiness = {}) {
    const expected = benchmarkProfile(candidate, transport);
    let last = null;
    let retryCount = 0;
    let monitorRecoveryCount = 0;
    let readinessStartedAt = performance.now();
    const ready = await poll(async () => {
      last = await this.snapshot();
      this.assertLeaseHealthy();
      const monitor = await this.ensureVirtualMonitorSelected(last);
      last = monitor.snapshot;
      if (monitor.reselected) {
        monitorRecoveryCount++;
        if (monitorRecoveryCount > 1)
          throw new StreamLabUnavailable("monitor-reconnect-loop",
            "Signaling repeatedly reconnected while waiting for the measured first frame.",
            { diagnostics: last });
        this.applyExpectedSource = await this.source.evaluate("window.__bubbleplaySource.triggerAction()");
        retryCount = 0;
        readinessStartedAt = performance.now();
        this.store.appendTelemetry({ phase: "monitor-reselected", at: new Date().toISOString(),
          stage: "first-frame", registeredAtMs: last?.signaling?.registeredAtMs ?? null,
          expectedTimestamp: this.applyExpectedSource?.epochMs ?? null });
        return false;
      }
      const state = firstFrameReadiness(last, { expected, preApplyBenchStatus: readiness.preApplyBenchStatus,
        runId: this.runId, transport, relay: readiness.relay,
        decoderPath: this.receiverOptions?.decoderPath,
        expectedTimestamp: this.applyExpectedSource?.epochMs });
      if (state.ready) return last;
      const elapsedMs = performance.now() - readinessStartedAt;
      if (shouldRetryMeasuredFrame(state, elapsedMs, retryCount)) {
        retryCount++;
        this.applyExpectedSource = await this.source.evaluate("window.__bubbleplaySource.triggerAction()");
        this.store.appendTelemetry({ phase: "first-frame-source-retry", at: new Date().toISOString(),
          retryCount, elapsedMs, reason: "primed-stream-still-stale",
          expectedTimestamp: this.applyExpectedSource?.epochMs ?? null });
      }
      if (elapsedMs >= 1_250 && state.rendered && state.routeOkay &&
          state.decoderOkay && state.benchOkay && !state.fresh) {
        const sourceMetadata = await this.source.evaluate("window.__bubbleplaySource.metadata()");
        const failure = classifySilentDdaFailure(
          state, last, this.applyExpectedSource, sourceMetadata, elapsedMs,
        );
        if (failure) {
          this.store.appendTelemetry({
            phase: failure.code,
            at: new Date().toISOString(),
            ...failure,
            expectedSource: this.applyExpectedSource,
            currentSource: sourceMetadata,
          });
          const message = failure.code === "wss-recovery-ack-stalled" ?
            "The browser presented the recovery frame but did not attempt its required WSS recovery acknowledgement." :
            "A verified moving source produced no DDA or encoded frames after the compatible WSS pipeline was primed.";
          throw new StreamLabUnavailable(
            failure.code,
            message,
            { failure, expectedSource: this.applyExpectedSource, currentSource: sourceMetadata, diagnostics: last },
          );
        }
      }
      return false;
    }, 15_000);
    if (!ready) {
      const diagnosticScreenshots = {};
      try {
        diagnosticScreenshots.source = join(this.store.path, "first-frame-source.png");
        diagnosticScreenshots.receiver = join(this.store.path, "first-frame-receiver.png");
        diagnosticScreenshots.osDisplay = join(this.store.path, "first-frame-os-display.png");
        await captureViewport(this.source, diagnosticScreenshots.source);
        await captureViewport(this.receiver, diagnosticScreenshots.receiver);
        captureDesktopRegion(this.display, diagnosticScreenshots.osDisplay);
        diagnosticScreenshots.allDisplays = [];
        for (const [index, display] of enumerateDisplays().entries()) {
          const path = join(this.store.path, `first-frame-os-display-${index}.png`);
          captureDesktopRegion(display, path);
          diagnosticScreenshots.allDisplays.push({ display, path });
        }
      } catch (error) {
        diagnosticScreenshots.error = String(error?.message || error);
      }
      const code = !last?.benchStatus || last.benchStatus.runId !== this.runId ? "benchmark-profile-unconfirmed" :
        (transport === "wss" ? last?.route !== "wss" : readiness.relay === true ? !isRelayRoute(last?.route) :
          readiness.relay === false ? !isPeerToPeerRoute(last?.route) : !isRtcRoute(last?.route)) ? "route-mismatch" :
        !(expected.codec === "h264" ? /h264|avc1/i : /hevc|h265|hev1/i).test(last?.dec ?? "") ? "codec-mismatch" : "first-frame-timeout";
      throw new StreamLabUnavailable(code, `The receiver did not confirm the requested route, ${expected.codec.toUpperCase()} profile, and first visible frame.`,
        { ...last, diagnosticScreenshots });
    }
    this.lastFirstFrameMs = performance.now() - this.firstFrameStartedAt;
    if (this.firstFrameMs === null) this.firstFrameMs = this.lastFirstFrameMs;
    return ready;
  }

  async waitForMediaPrimed(candidate, transport, readiness = {}) {
    const expected = benchmarkProfile(candidate, transport);
    let last = null;
    const primed = await poll(async () => {
      last = await this.snapshot();
      this.assertLeaseHealthy();
      const monitor = await this.ensureVirtualMonitorSelected(last);
      last = monitor.snapshot;
      if (monitor.reselected)
        this.store?.appendTelemetry?.({ phase: "monitor-reselected", at: new Date().toISOString(),
          stage: "media-prime", registeredAtMs: last?.signaling?.registeredAtMs ?? null });
      const tileAdmission = hevcTileDecoderAdmission(last, expected);
      if (tileAdmission.terminal) {
        throw new StreamLabUnavailable(
          "hevc-tile-decoder-unsupported",
          "The receiver cannot decode the bounded 128x128 HEVC Main tile stream; base video remains available.",
          { capability: tileAdmission, diagnostics: last },
        );
      }
      if (tileAdmission.required && !tileAdmission.ready) return false;
      const state = firstFrameReadiness(last, { expected, preApplyBenchStatus: readiness.preApplyBenchStatus,
        runId: this.runId, transport, relay: readiness.relay,
        decoderPath: this.receiverOptions?.decoderPath, expectedTimestamp: null });
      return state.rendered && state.routeOkay && state.decoderOkay && state.benchOkay ? last : false;
    }, 10_000);
    if (!primed) throw new StreamLabUnavailable("media-prime-timeout",
      "The receiver did not finish priming its route, decoder, benchmark profile, and startup frame before measurement.",
      { diagnostics: last });
    return primed;
  }

  async waitForStableWssCohort(stage, options = {}) {
    if (this.transport !== "wss") return null;
    const startedAt = performance.now();
    const timeoutMs = Number(options.timeoutMs ?? 10_000);
    const intervalMs = Number(options.intervalMs ?? 250);
    let state = null;
    let last = null;
    const admitted = await poll(async () => {
      this.assertLeaseHealthy();
      last = await this.snapshot();
      state = advanceWssCohortAdmission(state, last, options);
      return state.accepted ? { snapshot: last, state } : false;
    }, timeoutMs, intervalMs);
    if (!admitted) {
      this.store.appendTelemetry({
        phase: "wss-cohort-admission",
        stage,
        result: "timeout",
        elapsedMs: performance.now() - startedAt,
        consecutiveProgressSamples: state?.consecutiveProgressSamples ?? 0,
        requiredProgressSamples: state?.requiredProgressSamples ??
          Number(options.requiredProgressSamples ?? 3),
        observation: state?.observation ?? null,
      });
      throw new StreamLabUnavailable(
        "wss-cohort-admission-timeout",
        `WSS lane progress did not remain recovery-free before ${stage}.`,
        {
          stage,
          elapsedMs: performance.now() - startedAt,
          consecutiveProgressSamples: state?.consecutiveProgressSamples ?? 0,
          requiredProgressSamples: state?.requiredProgressSamples ??
            Number(options.requiredProgressSamples ?? 3),
          observation: state?.observation ?? null,
          diagnostics: last,
        },
      );
    }
    const result = {
      snapshot: admitted.snapshot,
      admittedAtMs: performance.now(),
      elapsedMs: performance.now() - startedAt,
      consecutiveProgressSamples: admitted.state.consecutiveProgressSamples,
      requiredProgressSamples: admitted.state.requiredProgressSamples,
      observation: admitted.state.observation,
    };
    this.store.appendTelemetry({
      phase: "wss-cohort-admission",
      stage,
      result: "accepted",
      elapsedMs: result.elapsedMs,
      consecutiveProgressSamples: result.consecutiveProgressSamples,
      requiredProgressSamples: result.requiredProgressSamples,
      observation: result.observation,
    });
    return result;
  }

  requireStableWssCohort(admission, snapshot, context = {}, options = {}) {
    if (!admission) return snapshot;
    const continuity = evaluateWssCohortContinuity(admission.snapshot, snapshot, options);
    if (continuity.accepted) return snapshot;
    this.store.appendTelemetry({
      phase: "wss-cohort-continuity",
      result: "lost",
      ...context,
      continuity,
    });
    throw new StreamLabUnavailable(
      "wss-cohort-lost",
      "The admitted WSS lane cohort entered recovery during exact-frame measurement.",
      {
        ...context,
        continuity,
        admitted: admission.snapshot,
        diagnostics: snapshot,
      },
    );
  }

  rebaseWssCohortAfterExpectedReset(admission, snapshot, context = {}) {
    const continuity = evaluateWssCohortContinuity(admission.snapshot, snapshot, {
      allowResyncChange: true,
    });
    if (continuity.accepted) return { snapshot };
    const transient = continuity.reasons.every(reason =>
      ["recovery-state-unhealthy", "rendered-frame-stale"].includes(reason));
    if (transient) return null;
    this.store.appendTelemetry({
      phase: "wss-cohort-continuity",
      result: "lost-during-expected-reset",
      ...context,
      continuity,
    });
    throw new StreamLabUnavailable(
      "wss-cohort-lost",
      "The admitted WSS lane cohort changed while applying the expected quality keyframe reset.",
      {
        ...context,
        continuity,
        admitted: admission.snapshot,
        diagnostics: snapshot,
      },
    );
  }

  snapshot() {
    return this.receiver.evaluate("window.__bubbleplayDiagnostics?.snapshot?.() ?? null");
  }

  assertLeaseHealthy() { this.assertCampaignActive(); this.heartbeat?.assertHealthy(); }

  async collectPerformance(seconds = 15, options = {}) {
    this.assertLeaseHealthy();
    const scene = options.scene ?? "motion-60";
    await this.source.evaluate(`window.__bubbleplaySource.setScene(${JSON.stringify(scene)},469); window.__bubbleplaySource.resume(); true`);
    await sleep(2_000);
    await this.waitForStableWssCohort("performance");
    await this.receiver.evaluate("window.__bubbleplayDiagnostics?.reset?.(); true");
    const samples = [];
    let started = performance.now();
    let systemAt = 0;
    let monitorRecoveries = 0;
    while (performance.now() - started < seconds * 1000) {
      this.assertLeaseHealthy();
      const elapsedMs = performance.now() - started;
      let snapshot = await this.snapshot();
      const monitor = await this.ensureVirtualMonitorSelected(snapshot);
      snapshot = monitor.snapshot;
      if (monitor.reselected) {
        monitorRecoveries++;
        if (monitorRecoveries > 1)
          throw new StreamLabUnavailable("monitor-reconnect-loop",
            "Signaling repeatedly reconnected while collecting the performance window.", { diagnostics: snapshot });
        samples.length = 0;
        started = performance.now();
        systemAt = 0;
        this.store.appendTelemetry({ phase: "monitor-reselected", at: new Date().toISOString(),
          stage: "performance", registeredAtMs: snapshot?.signaling?.registeredAtMs ?? null });
        continue;
      }
      if (snapshot) {
        const sourceMetadata = await this.source.evaluate("window.__bubbleplaySource.metadata()");
        const frameTrace = performanceFrameTrace(sourceMetadata, snapshot);
        const sample = { elapsedMs, ...snapshot, frameTrace };
        if (elapsedMs >= systemAt) { sample.system = sampleSystemTelemetry(); systemAt += 1_000; }
        samples.push(sample);
        this.store.appendTelemetry({ phase: "performance", ...sample });
      }
      await sleep(250);
    }
    let actionToPhotonSamples = [];
    let actionProbe = { result: "complete" };
    if (options.actionProbe === false) return { samples, actionToPhotonSamples,
      actionProbe: { result: "skipped", reason: "input-automation-disabled" } };
    try { actionToPhotonSamples = await this.measureActionToPhoton(3); }
    catch (error) {
      if (!['diagnostic-input', 'source-window-activation'].includes(error?.code)) throw error;
      actionProbe = { result: "unsupported", code: error.code, message: error.message };
      this.store.appendTelemetry({ phase: "action-to-photon", at: new Date().toISOString(), ...actionProbe });
    }
    return { samples, actionToPhotonSamples, actionProbe };
  }

  async measureActionToPhoton(count) {
    const measured = [];
    await this.source.evaluate("window.__bubbleplaySource.setScene('localized-change',469); window.__bubbleplaySource.resume(); true");
    activateProcessWindow(this.sourceProcess.pid);
    await this.receiver.evaluate("document.querySelector('#input-surface')?.focus(); true");
    for (let index = 0; index < count; index++) {
      this.assertLeaseHealthy();
      const monitor = await this.ensureVirtualMonitorSelected();
      if (monitor.reselected)
        this.store.appendTelemetry({ phase: "monitor-reselected", at: new Date().toISOString(),
          stage: "action-to-photon", registeredAtMs: monitor.snapshot?.signaling?.registeredAtMs ?? null });
      const before = await this.source.evaluate("window.__bubbleplaySource.metadata()");
      const started = performance.now();
      await this.receiver.call("Input.dispatchKeyEvent", { type: "keyDown", key: "F8", code: "F8", windowsVirtualKeyCode: 119, nativeVirtualKeyCode: 119 });
      await this.receiver.call("Input.dispatchKeyEvent", { type: "keyUp", key: "F8", code: "F8", windowsVirtualKeyCode: 119, nativeVirtualKeyCode: 119 });
      const triggered = await poll(async () => {
        const metadata = await this.source.evaluate("window.__bubbleplaySource.metadata()");
        return metadata.actionId > before.actionId ? metadata : false;
      }, 2_000);
      if (!triggered) throw new StreamLabUnavailable("diagnostic-input", "The reserved F8 action did not reach the isolated source display.", { stage: "input-trigger", diagnostics: await this.snapshot() });
      const presented = await poll(async () => {
        const snapshot = await this.snapshot();
        return sequenceReached(snapshot?.glassToGlass?.sequence, triggered.frameId & 0xffff) &&
          timestampReached(snapshot?.glassToGlass?.timestamp, triggered.epochMs >>> 0) ? snapshot : false;
      }, 5_000);
      if (!presented) throw new StreamLabUnavailable("action-presentation", "The action marker did not reach the decoded output.", { stage: "action-presentation", diagnostics: await this.snapshot() });
      measured.push(performance.now() - started);
      await sleep(250);
    }
    return measured;
  }

  async collectQuality(candidateId, sceneIds = [...SCENES.map(scene => scene.id), ...HYBRID_SCENES], options = {}) {
    const samplesRoot = join(this.store.path, "samples", safeName(candidateId));
    mkdirSync(samplesRoot, { recursive: true });
    await this.receiver.evaluate(`(() => { let style=document.querySelector("#stream-lab-capture-style"); if(!style){style=document.createElement("style");style.id="stream-lab-capture-style";style.textContent="#top-seam,#controls,#control-sheet,#touch-cursor,#desktop-state{display:none!important}";document.head.append(style)} return true })()`);
    const scores = [];
    const capturePlan = buildTemporalCapturePlan(sceneIds, {
      maxSamples: options.maxSamples ?? 12,
      deltaMode: options.deltaMode === true,
    });
    this.store.appendTelemetry({ phase: "quality", candidateId, pass: "start", samples: capturePlan.length });
    await this.source.evaluate(
      "window.__bubbleplaySource.setScene('motion-60',469); window.__bubbleplaySource.resume(); true",
    );
    await sleep(500);
    let qualityCohort = await this.waitForStableWssCohort("quality");
    for (const { scene, frameId: targetFrame, workload } of capturePlan) {
      this.assertLeaseHealthy();
      const initialMonitor = await this.ensureVirtualMonitorSelected();
      this.requireStableWssCohort(qualityCohort, initialMonitor.snapshot, {
        stage: "quality-frame-start",
        candidateId,
        scene,
        frameId: targetFrame,
      });
      if (initialMonitor.reselected)
        this.store.appendTelemetry({ phase: "monitor-reselected", at: new Date().toISOString(),
          stage: "quality-frame", candidateId, scene, frameId: targetFrame,
          registeredAtMs: initialMonitor.snapshot?.signaling?.registeredAtMs ?? null });
      let metadata = await prepareQualitySourceFrame(this.source, scene, targetFrame);
      if (!metadata) throw new StreamLabUnavailable("source-frame-timeout", `Source scene ${scene} did not pause at frame ${targetFrame}.`);
      const requestQualityKeyframe = requestId => requestCleanQualityKeyframe(
        this.receiver,
        requestId,
        { label: `${scene}:${targetFrame}` },
      );
      let qualityRequestId = `quality-${randomUUID()}`;
      let qualityAck = await requestQualityKeyframe(qualityRequestId);
      let alignmentOptions =
        qualityAnchorAlignmentOptions(options.hybridMode, scene, qualityAck.sequence);
      let alignment = null;
      let matched = null;
      let retryCount = 0;
      let monitorRecoveryCount = 0;
      let lastAlignmentFailure = null;
      let frameCohort = null;
      let matchStarted = performance.now();
      while (!matched && performance.now() - matchStarted < 5_000) {
        let snapshot = await this.snapshot();
        const monitor = await this.ensureVirtualMonitorSelected(snapshot);
        snapshot = monitor.snapshot;
        if (!frameCohort) {
          frameCohort = this.rebaseWssCohortAfterExpectedReset(
            qualityCohort,
            snapshot,
            {
              stage: "quality-anchor-rebase",
              candidateId,
              scene,
              frameId: targetFrame,
            },
          );
          if (!frameCohort) {
            await sleep(100);
            continue;
          }
        }
        this.requireStableWssCohort(frameCohort, snapshot, {
          stage: "quality-anchor",
          candidateId,
          scene,
          frameId: targetFrame,
        });
        if (monitor.reselected) {
          monitorRecoveryCount++;
          if (monitorRecoveryCount > 1)
            throw new StreamLabUnavailable("monitor-reconnect-loop",
              `Signaling repeatedly reconnected while matching frame ${scene}:${targetFrame}.`,
              { source: metadata, diagnostics: snapshot });
          this.store.appendTelemetry({ phase: "monitor-reselected", at: new Date().toISOString(),
            stage: "quality-match", candidateId, scene, frameId: targetFrame,
            registeredAtMs: snapshot?.signaling?.registeredAtMs ?? null });
          qualityRequestId = `quality-${randomUUID()}`;
          qualityAck = await requestQualityKeyframe(qualityRequestId);
          alignmentOptions =
            qualityAnchorAlignmentOptions(options.hybridMode, scene, qualityAck.sequence);
          lastAlignmentFailure = null;
          frameCohort = null;
          matchStarted = performance.now();
          await sleep(100);
          continue;
        }
        try {
          alignment = exactFrameAlignment(metadata, snapshot, "quality", alignmentOptions);
          matched = snapshot;
          break;
        } catch (error) {
          lastAlignmentFailure = summarizeAlignmentFailure(error);
          const elapsedMs = performance.now() - matchStarted;
          if (shouldRetryQualityFrameMatch(snapshot, elapsedMs, retryCount)) {
            retryCount += 1;
            const retryMonitor = await this.ensureVirtualMonitorSelected(snapshot);
            if (retryMonitor.reselected)
              this.store.appendTelemetry({ phase: "monitor-reselected", at: new Date().toISOString(),
                stage: "quality-retry", candidateId, scene, frameId: targetFrame,
                registeredAtMs: retryMonitor.snapshot?.signaling?.registeredAtMs ?? null });
            qualityRequestId = `quality-${randomUUID()}`;
            this.store.appendTelemetry({
              phase: "quality-frame-retry",
              candidateId,
              scene,
              frameId: targetFrame,
              retry: retryCount,
              elapsedMs,
            });
            qualityAck = await requestQualityKeyframe(qualityRequestId);
            alignmentOptions =
              qualityAnchorAlignmentOptions(options.hybridMode, scene, qualityAck.sequence);
            lastAlignmentFailure = null;
            frameCohort = null;
          }
        }
        if (!matched) await sleep(100);
      }
      if (!matched) {
        throw new StreamLabUnavailable(
          "frame-match-timeout",
          `Decoded output never presented matched frame ${scene}:${targetFrame}.`,
          { source: metadata, lastAlignmentFailure, diagnostics: await this.snapshot() },
        );
      }
      let anchor = null;
      let trigger = null;
      if (options.deltaMode === true) {
        anchor = { metadata, alignment };
        this.store.appendTelemetry({ phase: "quality-frame-trace", candidateId, scene,
          frameId: targetFrame, checkpoint: "anchor", ...alignment });
        if (await armPresentedIdentityHistory(this.receiver, 16) !== true)
          throw new StreamLabUnavailable("receiver-compatibility-mismatch",
            "The receiver cannot record bounded exact delta-frame identity history.");
        const deltaMetadata = await prepareQualityDeltaFrame(this.source, metadata, {
          triggerFrames: qualityDeltaTriggerFrames(options),
          waitForTrigger: async (triggerMetadata) => {
            let triggerAlignment = null;
            let lastTriggerAlignmentFailure = null;
            const triggerStarted = performance.now();
            while (!triggerAlignment && performance.now() - triggerStarted < 5_000) {
              let snapshot = await this.snapshot();
              const monitor = await this.ensureVirtualMonitorSelected(snapshot);
              snapshot = monitor.snapshot;
              this.requireStableWssCohort(frameCohort, snapshot, {
                stage: "quality-delta-trigger",
                candidateId,
                scene,
                frameId: triggerMetadata.frameId,
              });
              if (monitor.reselected)
                throw new StreamLabUnavailable("quality-delta-trigger-monitor-reconnect",
                  `Signaling reconnected while matching delta trigger ${scene}:${triggerMetadata.frameId}.`,
                  { source: triggerMetadata, diagnostics: snapshot });
              try {
                triggerAlignment = exactFrameAlignment(
                  triggerMetadata,
                  snapshot,
                  "quality-delta-trigger",
                  {
                    afterNetworkSequence: trigger?.alignment?.trace?.network?.sequence ??
                      qualityAck.sequence,
                    requireDeltaFrame: true,
                  },
                );
              } catch (error) {
                lastTriggerAlignmentFailure = summarizeAlignmentFailure(error);
              }
              if (!triggerAlignment) await sleep(50);
            }
            if (!triggerAlignment)
              throw new StreamLabUnavailable("quality-delta-trigger-timeout",
                `Decoded output never presented the exact delta trigger ${scene}:${triggerMetadata.frameId}.`,
                { source: triggerMetadata, anchor: anchor.metadata,
                  lastAlignmentFailure: lastTriggerAlignmentFailure, diagnostics: await this.snapshot() });
            trigger = { metadata: triggerMetadata, alignment: triggerAlignment };
            this.store.appendTelemetry({ phase: "quality-frame-trace", candidateId, scene,
              frameId: triggerMetadata.frameId, checkpoint: "trigger", ...triggerAlignment });
            return true;
          },
        });
        if (!deltaMetadata)
          throw new StreamLabUnavailable("quality-delta-source-timeout",
            `Source scene ${scene} did not advance through its trigger to the scored delta frame.`,
            { anchor: metadata, trigger: trigger?.metadata ?? null });
        metadata = deltaMetadata;
        const hybridRequirement = hybridSceneRequirement(
          options.hybridMode,
          scene,
          options.hybridCacheMode,
        );
        alignmentOptions = {
          afterNetworkSequence: trigger.alignment.trace.network.sequence,
          requireDeltaFrame: true,
          requireRoiApplied: options.requireRoiActivity === true,
          ...(hybridRequirement !== "off" ? { hybridRequirement } :
            { requireHybridTiles: options.requireHybridActivity === true }),
        };
        alignment = null;
        matched = null;
        let lastDeltaAlignmentFailure = null;
        matchStarted = performance.now();
        while (!matched && performance.now() - matchStarted < 5_000) {
          let snapshot = await this.snapshot();
          const monitor = await this.ensureVirtualMonitorSelected(snapshot);
          snapshot = monitor.snapshot;
          this.requireStableWssCohort(frameCohort, snapshot, {
            stage: "quality-delta",
            candidateId,
            scene,
            frameId: metadata.frameId,
          });
          if (monitor.reselected)
            throw new StreamLabUnavailable("quality-delta-monitor-reconnect",
              `Signaling reconnected while matching delta frame ${scene}:${metadata.frameId}.`,
              { source: metadata, diagnostics: snapshot });
          try {
            alignment = exactFrameAlignment(metadata, snapshot, "quality-delta", alignmentOptions);
            matched = snapshot;
          } catch (error) {
            lastDeltaAlignmentFailure = summarizeAlignmentFailure(error);
          }
          if (!matched) await sleep(50);
        }
        if (!matched)
          throw new StreamLabUnavailable("quality-delta-frame-timeout",
            `Decoded output never presented the exact mapped delta frame ${scene}:${metadata.frameId}.`,
            { source: metadata, anchor: anchor.metadata, trigger: trigger.metadata,
              lastAlignmentFailure: lastDeltaAlignmentFailure,
              diagnostics: await this.snapshot() });
      }
      this.store.appendTelemetry({ phase: "quality-frame-trace", candidateId, scene, frameId: metadata.frameId,
        checkpoint: "preCapture", ...alignment });
      const sampleStem = qualitySampleStem(scene, metadata);
      const referencePath = join(samplesRoot, `${sampleStem}-reference.png`);
      const decodedPath = join(samplesRoot, `${sampleStem}-decoded.png`);
      const capturedDiagnostics = await captureReceiverFrame(this.receiver, decodedPath, {
        source: metadata,
        phase: "quality-capture",
        alignmentOptions,
        timeoutMs: 5_000,
      });
      this.requireStableWssCohort(frameCohort, capturedDiagnostics, {
        stage: "quality-capture",
        candidateId,
        scene,
        frameId: metadata.frameId,
      });
      const capturedAlignment = exactFrameAlignment(metadata, capturedDiagnostics, "quality-capture", {
        ...alignmentOptions,
        allowHistoricalDelta: false,
      });
      this.store.appendTelemetry({ phase: "quality-frame-trace", candidateId, scene, frameId: metadata.frameId,
        checkpoint: "capture", ...capturedAlignment });
      await captureViewport(this.source, referencePath);
      const postSource = await this.source.evaluate("window.__bubbleplaySource.metadata()");
      const postSnapshot = await this.snapshot();
      this.requireStableWssCohort(frameCohort, postSnapshot, {
        stage: "quality-post-capture",
        candidateId,
        scene,
        frameId: metadata.frameId,
      });
      const postCapture = exactFrameAlignment(postSource, postSnapshot, "quality-post-capture");
      if (postCapture.identity.generation !== alignment.identity.generation ||
          postCapture.identity.frameId !== alignment.identity.frameId ||
          postCapture.identity.timestamp !== alignment.identity.timestamp)
        throw new StreamLabUnavailable("frame-changed-during-capture", "The selected source/decoded frame changed while screenshots were captured.",
          { before: alignment.identity, postCapture: postCapture.identity });
      const score = scoreImagePair(referencePath, decodedPath, join(samplesRoot, sampleStem));
      this.assertLeaseHealthy();
      const sample = { scene, workload, generation: metadata.generation, frameId: metadata.frameId,
        anchorFrameId: anchor?.metadata?.frameId ?? null,
        timestamp: metadata.epochMs >>> 0, exactAligned: true, referencePath, decodedPath,
        roiActivity: capturedAlignment.roiActivity,
        hybridActivity: capturedAlignment.hybridActivity,
        hybridDiagnostics: capturedDiagnostics?.wssMedia?.hybrid ?? null,
        frameTrace: {
          ...(anchor ? { anchor: anchor.alignment.trace } : {}),
          ...(trigger ? { trigger: trigger.alignment.trace } : {}),
          preCapture: alignment.trace,
          capture: capturedAlignment.trace,
          postCapture: postCapture.trace,
        }, ...score };
      scores.push(sample);
      writeFileSync(join(samplesRoot, "scores.partial.json"), `${JSON.stringify(scores, null, 2)}\n`);
      this.store.appendTelemetry({ phase: "quality-frame-trace", candidateId, scene, frameId: metadata.frameId,
        checkpoint: "postCapture", postCapture, metrics: scoreForTelemetry(score) });
      qualityCohort = { ...qualityCohort, snapshot: postSnapshot };
    }
    const localized = scores.filter(score => score.scene === "localized-change").sort((a, b) => a.frameId - b.frameId);
    let temporal = null;
    if (localized.length >= 2) {
      const refA = readImageLuma(localized[0].referencePath), refB = readImageLuma(localized[1].referencePath);
      const decA = readImageLuma(localized[0].decodedPath), decB = readImageLuma(localized[1].decodedPath);
      temporal = temporalUnchangedRegionError(refA.luma, refB.luma, decA.luma, decB.luma, refA.width, refA.height);
      localized.at(-1).unchangedRegionError = temporal.mean;
    }
    const paritySamples = scores.map(score => ({
      sourceFrameId: score.generation * 131_072 + score.frameId,
      exactFrame: score.exactAligned,
      vmaf: score.vmaf, ssim: score.ssim, worstTileSsim: score.tileSsim?.worst,
      edgeFidelity: score.edgeFidelity, colorError: score.colorError, ocrCer: score.ocrCer,
      unchangedRegionError: score.unchangedRegionError,
      blockArtifactSeverity: score.blockArtifactSeverity,
      presentedAtMs: score.frameTrace?.capture?.presentation?.presentedAtMs,
    }));
    const parityVector = aggregateParityVector(paritySamples, { expectedFrameIds: paritySamples.map(sample => sample.sourceFrameId) });
    const retainedScores = retainWorstSamplePairs(scores, { limit: options.retainWorst ?? 10 });
    writeFileSync(join(samplesRoot, "scores.json"), `${JSON.stringify(retainedScores, null, 2)}\n`);
    this.assertLeaseHealthy();
    return { ...summarizeQuality(scores, temporal), ...parityVector,
      capturePlan, frameTraces: scores.map(score => ({ scene: score.scene, frameId: score.frameId, ...score.frameTrace })),
      mappedQualitySamples: scores.filter(score => score.roiActivity?.reason === "applied").length,
      hybridQualitySamples: scores.filter(score => Number(score.hybridActivity?.tilesDrawn) > 0).length,
      hybridGuardSamples: scores.filter(score =>
        score.hybridActivity?.requirement === "guard" &&
        Number(score.hybridActivity?.tilesDrawn) === 0).length,
      hybridActivity: {
        composites: Math.max(0, ...scores.map(score =>
          Number(score.hybridDiagnostics?.composites) || 0)),
        decodedTiles: Math.max(0, ...scores.map(score =>
          Number(score.hybridDiagnostics?.decodedTiles) || 0)),
        decoderErrors: Math.max(0, ...scores.map(score =>
          Number(score.hybridDiagnostics?.decoderErrors) || 0)),
        decoderPressureBypasses: Math.max(0, ...scores.map(score =>
          Number(score.hybridDiagnostics?.decoderPressureBypasses) || 0)),
        hevcBytes: Math.max(0, ...scores.map(score =>
          Number(score.hybridDiagnostics?.hevcBytes) || 0)),
        staleDrawRejects: Math.max(0, ...scores.map(score =>
          Number(score.hybridDiagnostics?.staleDrawRejects) || 0)),
        staleComposites: Math.max(0, ...scores.map(score =>
          Number(score.hybridDiagnostics?.staleComposites) || 0)),
      },
      qualityActivity: scores.map(score => ({ scene: score.scene, frameId: score.frameId,
        anchorFrameId: score.anchorFrameId, roiActivity: score.roiActivity,
        hybridActivity: score.hybridActivity })),
      retainedPairs: retainedScores.filter(score => score.retained).length };
  }

  async collectDecoderFaultRecovery() {
    this.assertLeaseHealthy();
    await this.source.evaluate(`window.__bubbleplaySource.setScene("motion-60",469); window.__bubbleplaySource.resume(); true`);
    await sleep(1_000);
    const before = await this.snapshot();
    const accepted = await this.receiver.evaluate("window.__bubbleplayDiagnostics?.injectDecoderFault?.() ?? false");
    if (!accepted) return unsupportedUnit("receiver-diagnostics", "The active decoder path did not expose deterministic fault injection.");
    const started = performance.now();
    const recovered = await poll(async () => {
      this.assertLeaseHealthy();
      const current = await this.snapshot();
      const resetAdvanced = Number(current?.resyncs ?? current?.worker?.resyncs) > Number(before?.resyncs ?? before?.worker?.resyncs);
      const frameAdvanced = current?.glassToGlass?.sequence !== before?.glassToGlass?.sequence;
      const healthy = !/waiting|recover/i.test(String(current?.dec || ""));
      return resetAdvanced && frameAdvanced && healthy ? current : false;
    }, 10_000);
    if (!recovered) throw new StreamLabUnavailable("decoder-fault-recovery-timeout", "The injected decoder reset did not return to visible output within 10 seconds.", { before, diagnostics: await this.snapshot() });
    return { result: "complete", recoveryMs: performance.now() - started,
      resets: Number(recovered.resyncs ?? recovered.worker?.resyncs) - Number(before.resyncs ?? before.worker?.resyncs),
      codec: recovered.dec, route: recovered.route };
  }

  async collectSharpViewportComparison(candidateId) {
    this.assertLeaseHealthy();
    const full = await this.collectQuality(`${candidateId}-full`, ["ui-text"]);
    const accepted = await this.receiver.evaluate("window.__bubbleplayDiagnostics?.setFixedZoom?.(2) ?? false");
    if (!accepted) return unsupportedUnit("receiver-diagnostics", "The receiver rejected deterministic 2× Work-mode zoom selection.");
    try {
      const committed = await poll(async () => {
        this.assertLeaseHealthy();
        const snapshot = await this.snapshot();
        return snapshot?.viewport?.gainX >= 1.8 && /^sharp/.test(snapshot?.viewport?.state || "") && !snapshot?.viewport?.pending ? snapshot : false;
      }, 10_000);
      if (!committed) return unsupportedUnit("media-view", "The host did not confirm a deterministic 2× sharp viewport crop.", { diagnostics: await this.snapshot() });
      const metadata = await this.source.evaluate("window.__bubbleplaySource.metadata()");
      const matched = await poll(async () => {
        this.assertLeaseHealthy();
        const snapshot = await this.snapshot();
        return snapshot?.glassToGlass?.sequence === (metadata.frameId & 0xffff) &&
          snapshot?.glassToGlass?.timestamp === (metadata.epochMs >>> 0) ? snapshot : false;
      }, 5_000);
      if (!matched) throw new StreamLabUnavailable("sharp-frame-match-timeout", "The 2× sharp viewport did not present the selected deterministic source frame.", { diagnostics: await this.snapshot() });
      const samplesRoot = join(this.store.path, "samples", safeName(`${candidateId}-sharp-2x`));
      mkdirSync(samplesRoot, { recursive: true });
      const referencePath = join(samplesRoot, "ui-text-reference.png");
      const decodedPath = join(samplesRoot, "ui-text-decoded.png");
      await captureViewport(this.source, referencePath, { x: 480, y: 270, width: 960, height: 540, scale: 2 });
      await captureViewport(this.receiver, decodedPath);
      const score = scoreImagePair(referencePath, decodedPath, join(samplesRoot, "ui-text"));
      this.assertLeaseHealthy();
      const sharp = summarizeQuality([{ ...score, scene: "ui-text", frameId: metadata.frameId }]);
      return { result: "complete", zoom: 2, viewport: committed.viewport, full, sharp,
        improvement: { ocrCer: full.ocrCer - sharp.ocrCer, vmaf: sharp.vmaf - full.vmaf,
          worstTileSsim: sharp.worstTileSsim - full.worstTileSsim } };
    } finally {
      await this.receiver.evaluate("window.__bubbleplayDiagnostics?.setFixedZoom?.(1) ?? false");
    }
  }

  async collectQuiescentInteractionTrials() {
    this.assertLeaseHealthy();
    await this.source.evaluate(
      "window.__bubbleplaySource.setScene('motion-60',469); window.__bubbleplaySource.resume(); true",
    );
    await sleep(500);
    let interactionCohort = await this.waitForStableWssCohort("quiescent-interactions");
    const previousHybrid = interactionCohort.snapshot?.wssMedia?.hybrid ?? {};
    const anchorMetadata = await prepareQualitySourceFrame(this.source, "source-code", 0);
    if (!anchorMetadata)
      throw new StreamLabUnavailable("quiescent-source-not-ready",
        "The deterministic source-code frame did not settle before interaction trials.");
    const anchorRequestId = `quality-${randomUUID()}`;
    const anchorAck = await requestCleanQualityKeyframe(
      this.receiver,
      anchorRequestId,
      { label: "quiescent interaction source-code:0" },
    );
    let anchorCohort = null;
    const anchor = await poll(async () => {
      const snapshot = await this.snapshot();
      if (!anchorCohort) {
        anchorCohort = this.rebaseWssCohortAfterExpectedReset(
          interactionCohort,
          snapshot,
          { stage: "quiescent-interaction-anchor-rebase" },
        );
        if (!anchorCohort) return false;
      }
      this.requireStableWssCohort(anchorCohort, snapshot, {
        stage: "quiescent-interaction-anchor",
      });
      try {
        const alignment = exactFrameAlignment(
          anchorMetadata,
          snapshot,
          "quiescent-interaction-anchor",
          qualityAnchorAlignmentOptions(
            "png-quiescent-detail",
            "source-code",
            anchorAck.sequence,
          ),
        );
        return { snapshot, alignment };
      } catch {
        return false;
      }
    }, 5_000);
    if (!anchor)
      throw new StreamLabUnavailable("quiescent-anchor-timeout",
        "The source-code anchor did not reach the receiver before interaction warm-up.",
        { sourceMetadata: anchorMetadata, anchorAck, diagnostics: await this.snapshot() });
    interactionCohort = anchorCohort;
    const waitForExactFrame = async (metadata, phase, afterNetworkSequence, requireDeltaFrame) =>
      poll(async () => {
        const snapshot = await this.snapshot();
        this.requireStableWssCohort(interactionCohort, snapshot, { stage: phase });
        try {
          const alignment = exactFrameAlignment(metadata, snapshot, phase, {
            ...(afterNetworkSequence === undefined ? {} : { afterNetworkSequence }),
            ...(requireDeltaFrame ? { requireDeltaFrame: true } : {}),
          });
          return { snapshot, alignment };
        } catch {
          return false;
        }
      }, 5_000);
    let lastNetworkSequence = anchor.alignment.trace.network.sequence;
    if (await armPresentedIdentityHistory(this.receiver, 16) !== true)
      throw new StreamLabUnavailable("receiver-compatibility-mismatch",
        "The receiver cannot record bounded interaction-frame identity history.");
    const sourceMetadata = await prepareQualityDeltaFrame(this.source, anchorMetadata, {
      triggerFrames: 3,
      waitForTrigger: async triggerMetadata => {
        const matched = await waitForExactFrame(
          triggerMetadata,
          "quiescent-interaction-warmup",
          lastNetworkSequence,
          true,
        );
        if (!matched)
          throw new StreamLabUnavailable("quiescent-warmup-timeout",
            "A sequence-keyed quiescence warm-up frame did not reach the receiver.",
            { sourceMetadata: triggerMetadata, diagnostics: await this.snapshot() });
        lastNetworkSequence = matched.alignment.trace.network.sequence;
        return true;
      },
    });
    if (!sourceMetadata)
      throw new StreamLabUnavailable("quiescent-source-not-ready",
        "The deterministic source-code warm-up did not produce a final scored frame.");
    await this.receiver.evaluate("window.__bubbleplayDiagnostics?.setFixedZoom?.(1) ?? false");
    const ready = await poll(async () => {
      const snapshot = await this.snapshot();
      this.requireStableWssCohort(interactionCohort, snapshot, {
        stage: "quiescent-interaction-ready",
      });
      try {
        exactFrameAlignment(sourceMetadata, snapshot, "quiescent-interaction-ready", {
          afterNetworkSequence: lastNetworkSequence,
          requireDeltaFrame: true,
        });
      } catch {
        return false;
      }
      return quiescentInteractionReady(snapshot, sourceMetadata, previousHybrid) ? snapshot : false;
    }, 10_000);
    if (!ready)
      throw new StreamLabUnavailable("quiescent-detail-not-ready",
        "The exact source-code frame did not receive fresh drawable detail before interaction trials.",
        { sourceMetadata, previousHybrid, diagnostics: await this.snapshot() });

    const moveCursor = async (x, y) => poll(
      () => this.receiver.evaluate(
        `window.__bubbleplayDiagnostics?.moveCursorNormalized?.(${x},${y}) ?? false`,
      ),
      1_000,
      50,
    );
    const cursorBefore = ready;
    for (const [x, y] of [[0.2, 0.2], [0.8, 0.2], [0.8, 0.8], [0.2, 0.8]]) {
      const moved = await moveCursor(x, y);
      if (!moved)
        throw new StreamLabUnavailable("quiescent-cursor-sweep-unavailable",
          "The receiver could not perform the deterministic 1× cursor sweep.");
      await sleep(75);
    }
    const cursorAfter = await poll(async () => {
      const snapshot = await this.snapshot();
      this.requireStableWssCohort(interactionCohort, snapshot, {
        stage: "quiescent-cursor-sweep",
      });
      return quiescentCursorSweepSettled(cursorBefore, snapshot, 4) ? snapshot : false;
    }, 5_000);
    if (!cursorAfter)
      throw new StreamLabUnavailable("quiescent-cursor-settle-timeout",
        "The 1× cursor-only sweep did not send all four packets and cross a newer host telemetry window.",
        { before: cursorBefore, diagnostics: await this.snapshot() });
    const cursorChanges = quiescentCursorCounterChanges(cursorBefore, cursorAfter);
    if (cursorChanges.length)
      throw new StreamLabUnavailable("quiescent-cursor-damage",
        "A settled 1× cursor-only sweep changed the enhancement plane.",
        { fields: cursorChanges, before: cursorBefore, after: cursorAfter });

    const transition = async (action, code) => {
      const before = await this.snapshot();
      const accepted = await action();
      if (!accepted)
        throw new StreamLabUnavailable(`${code}-unavailable`,
          "The receiver rejected a deterministic quiescent-detail geometry transition.");
      const after = await poll(async () => {
        const snapshot = await this.snapshot();
        this.requireStableWssCohort(
          interactionCohort,
          snapshot,
          { stage: code },
          { allowMediaGenerationChange: true },
        );
        return quiescentTransitionCommitted(before, snapshot) ? snapshot : false;
      }, 5_000);
      if (!after)
        throw new StreamLabUnavailable(`${code}-timeout`,
          "Base video transitioned but quiescent detail was not restored.",
          { before, diagnostics: await this.snapshot() });
      const baseOnlyDelta = Number(after.wssMedia.hybrid.baseOnlyTransitionFrames) -
        Number(before.wssMedia.hybrid.baseOnlyTransitionFrames);
      if (baseOnlyDelta > 1 || Number(after.wssMedia.hybrid.staleComposites) !== 0)
        throw new StreamLabUnavailable(`${code}-ordering`,
          "The geometry transition exceeded the one-frame base-only or stale-composite gate.",
          { baseOnlyDelta, before, after });
      interactionCohort = { ...interactionCohort, snapshot: after };
      return { baseOnlyFrames: baseOnlyDelta, cropRevision: after.wssMedia.hybrid.cropRevision };
    };

    const zoomIn = await transition(
      () => this.receiver.evaluate("window.__bubbleplayDiagnostics?.setFixedZoom?.(2) ?? false"),
      "quiescent-zoom-in",
    );
    const pan = await transition(
      () => this.receiver.evaluate("window.__bubbleplayDiagnostics?.panFixedViewport?.(180,90) ?? false"),
      "quiescent-pan",
    );
    const cursorFollow = await transition(
      () => moveCursor(0.95, 0.5),
      "quiescent-cursor-follow",
    );
    const zoomOut = await transition(
      () => this.receiver.evaluate("window.__bubbleplayDiagnostics?.setFixedZoom?.(1) ?? false"),
      "quiescent-zoom-out",
    );
    const report = { accepted: true, cursorNeutral: true, zoomIn, pan, cursorFollow, zoomOut };
    this.store.appendTelemetry({ phase: "quiescent-interactions", ...report });
    return report;
  }

  async applyAdaptiveShape(bandwidthMbps, seconds) {
    this.assertLeaseHealthy();
    return unsupportedUnit("candidate-network-shaper",
      `Local candidate adaptive shaping is not available yet (${bandwidthMbps} Mbps for ${seconds}s).`);
  }

  async collectH264FallbackSmoke(seconds) {
    this.assertLeaseHealthy();
    try {
      this.candidateProfileLease = null;
      rmSync(this.candidateProfilePath, { force: true });
    }
    catch (error) { return unsupportedUnit("candidate-profile-release", `The local candidate profile could not be released: ${error.message}`); }
    const wssReady = await poll(async () => {
      this.assertLeaseHealthy();
      return (await this.snapshot())?.route === "wss";
    }, 15_000);
    if (!wssReady) throw new StreamLabUnavailable("h264-fallback-route-timeout",
      "The WSS route did not start before requesting H264 fallback.");
    await this.receiver.evaluate("window.__bubbleplayDiagnostics?.setStreamMode?.('work') ?? false");
    const accepted = await this.receiver.evaluate("window.__bubbleplayDiagnostics?.forceH264Fallback?.() ?? false");
    if (!accepted) return unsupportedUnit("receiver-diagnostics", "The receiver rejected the one-way H264 fallback control.");
    this.firstFrameStartedAt = performance.now();
    this.applyExpectedSource = await this.source.evaluate("window.__bubbleplaySource.triggerAction()");
    await this.selectVirtualMonitor();
    const ready = await poll(async () => {
      this.assertLeaseHealthy();
      const monitor = await this.ensureVirtualMonitorSelected(await this.snapshot());
      const snapshot = monitor.snapshot;
      if (monitor.reselected) this.applyExpectedSource = await this.source.evaluate("window.__bubbleplaySource.triggerAction()");
      const fresh = timestampReached(snapshot?.glassToGlass?.timestamp, this.applyExpectedSource?.epochMs >>> 0);
      return snapshot?.route === "wss" && /h264|avc1/i.test(snapshot?.dec ?? "") && fresh ? snapshot : false;
    }, 15_000);
    if (!ready) throw new StreamLabUnavailable("h264-fallback-timeout", "The production WSS route did not present a fresh H264 fallback frame.", { diagnostics: await this.snapshot() });
    this.lastFirstFrameMs = performance.now() - this.firstFrameStartedAt;
    const performanceSample = await this.collectPerformance(seconds);
    const samples = performanceSample.samples.map(sample => ({ ...sample, benchStatus: null }));
    return { ...summarizePerformance(samples, { transport: "wss", firstFrameMs: this.lastFirstFrameMs,
      actionToPhotonSamples: performanceSample.actionToPhotonSamples }), result: "complete", codec: "h264" };
  }

  async collectWebRtcDiagnosticSmoke(seconds, { relay = false } = {}) {
    this.assertLeaseHealthy();
    // Benchmark leases are HEVC-only; this measures the production H264 path.
    this.candidateProfileLease = null;
    rmSync(this.candidateProfilePath, { force: true });
    await this.switchTransport("webrtc", { relay, adaptive: false });
    await this.receiver.evaluate("window.__bubbleplayDiagnostics?.setStreamMode?.('game') ?? false");
    await this.selectVirtualMonitor();
    this.firstFrameStartedAt = performance.now();
    this.applyExpectedSource = await this.source.evaluate("window.__bubbleplaySource.triggerAction()");
    const ready = await poll(async () => {
      this.assertLeaseHealthy();
      const monitor = await this.ensureVirtualMonitorSelected(await this.snapshot());
      const snapshot = monitor.snapshot;
      if (monitor.reselected) this.applyExpectedSource = await this.source.evaluate("window.__bubbleplaySource.triggerAction()");
      return /^(host|srflx|prflx|relay)->(host|srflx|prflx|relay)$/.test(snapshot?.route ?? "") &&
        /^avc1\./i.test(snapshot?.worker?.codec ?? "") &&
        snapshot?.renderedAgeMs < 1000 && snapshot?.glassToGlass?.ageMs < 1000 &&
        timestampReached(snapshot?.glassToGlass?.timestamp, this.applyExpectedSource?.epochMs >>> 0) ? snapshot : false;
    }, 15_000);
    if (!ready) throw new StreamLabUnavailable("webrtc-diagnostic-timeout",
      "The H264 WebRTC diagnostic path did not present a fresh frame.", { diagnostics: await this.snapshot() });
    this.lastFirstFrameMs = performance.now() - this.firstFrameStartedAt;
    const sample = await this.collectPerformance(seconds, { actionProbe: false });
    if (!webRtcDiagnosticProgress(sample.samples)) throw new StreamLabUnavailable("webrtc-diagnostic-stalled",
      "The H264 WebRTC diagnostic path did not sustain fresh presentation.", { diagnostics: await this.snapshot() });
    return { ...summarizePerformance(sample.samples.map(value => ({ ...value, benchStatus: null })),
      { transport: "webrtc", firstFrameMs: this.lastFirstFrameMs }), result: "complete", codec: "h264",
      decoderPath: "webcodecs", measurement: "compatibility", benchmarkOverride: false };
  }

  stop(options = {}) {
    if (this.stopPromise) return this.stopPromise;
    this.stopPromise = this.performStop(options);
    return this.stopPromise;
  }

  async performStop(options = {}) {
    this.stopped = true;
    if (!options.fromSignal) { this.disposeTermination?.(); this.disposeTermination = null; }
    const children = [
      { role: "receiver", pid: this.receiverProcess?.pid },
      { role: "source", pid: this.sourceProcess?.pid },
      { role: "candidate-host", pid: this.candidateProcess?.pid },
    ];
    const shutdown = await Promise.all([
      shutdownChrome(this.receiver, this.receiverProcess),
      shutdownChrome(this.source, this.sourceProcess),
      shutdownChild(this.candidateProcess),
    ]);
    for (const remove of this.receiverWebSocketDiagnosticsDispose) remove();
    this.receiverWebSocketDiagnosticsDispose = [];
    this.receiver?.close(); this.source?.close();
    let heartbeatError = null;
    try { await this.heartbeat?.stop(); }
    catch (error) { heartbeatError = error; }
    let helperError = null;
    if (this.helperSession) {
      try { await this.helperSession.end(this.runId); }
      catch (error) { helperError = error; }
      this.helperSession = null;
    }
    await sleep(250);
    if (this.preRunDisplays && this.display) {
      const deadline = Date.now() + 10_000;
      do {
        this.cleanupEvidence = inspectCleanupResources({
          preDisplays: this.preRunDisplays,
          transientDisplay: this.display,
          persistentDisplay: !this.transientVdd,
          childProcesses: children,
        });
        if (cleanupResourcesReleased(this.cleanupEvidence) || Date.now() >= deadline) break;
        await sleep(250);
      } while (true);
      this.cleanupEvidence.shutdown = { receiver: shutdown[0], source: shutdown[1], candidateHost: shutdown[2] };
    } else {
      this.cleanupEvidence = { evidenceAvailability: { service: false, displays: false, qos: false, chrome: false },
        reason: "session-start-did-not-complete" };
    }
    try { rmSync(this.runScratch, { recursive: true, force: true, maxRetries: 3 }); } catch {}
    if (heartbeatError) throw heartbeatError;
    if (helperError) throw helperError;
  }
}

function cleanupResourcesReleased(evidence) {
  return (evidence?.displayResourcesClean ?? evidence?.transientVddAbsent) &&
    evidence?.qosPoliciesAbsent && evidence?.chromeChildrenExited;
}

function unsupportedUnit(boundary, reason, detail = null) {
  return { result: "unsupported", boundary, reason, ...(detail ? { detail } : {}) };
}

function benchTransport(status) {
  return status?.effective?.transport ?? status?.transport ?? status?.metrics?.transport ?? null;
}

function isPeerToPeerRoute(route) {
  const value = String(route || "").toLowerCase();
  return /^(host|srflx|prflx)->(host|srflx|prflx)$/.test(value);
}

function isRelayRoute(route) { return String(route || "").toLowerCase().split("->").includes("relay"); }
function isRtcRoute(route) { return /^(host|srflx|prflx|relay)->(host|srflx|prflx|relay)$/.test(String(route || "").toLowerCase()); }

function benchMatches(status, expected) {
  const effective = status?.effective ?? status;
  const encoder = effective?.encoder ?? effective;
  if (!encoder) return false;
  const common = encoder.preset === expected.encoder.preset && encoder.tuning === expected.encoder.tuning &&
    encoder.multipass === expected.encoder.multipass && encoder.aqStrength === expected.encoder.aqStrength &&
    encoder.vbvFrames === expected.encoder.vbvFrames &&
    effective.transport === expected.transport && effective.codec === expected.codec &&
    effective.streamMode === expected.streamMode && effective.width === expected.width &&
    effective.height === expected.height && effective.fps === expected.fps &&
    effective.bitrateBps === expected.bitrateBps && effective.gopSeconds === expected.gopSeconds;
  if (!common) return false;
  if (expected.schemaVersion === 2 && ["feedback", "feedback-headroom"].includes(expected.encoder.roiMode) &&
      status?.roiActivity?.reconstructionActive !== true) return false;
  if (expected.schemaVersion === 2) return status.protocol === 2 &&
    effective.implementationVariant === expected.implementationVariant && effective.experimentId === expected.experimentId &&
    (effective.hybridMode ?? "off") === expected.hybridMode &&
    (effective.hybridCacheMode ?? "suppress-reactivate") === expected.hybridCacheMode &&
    encoder.aqMode === expected.encoder.aqMode && encoder.lookaheadDepth === expected.encoder.lookaheadDepth &&
    encoder.weightedPrediction === expected.encoder.weightedPrediction &&
    encoder.dirtyQpMap === expected.encoder.dirtyQpMap &&
    encoder.main10 === expected.encoder.main10 &&
    (encoder.roiMode ?? (encoder.dirtyQpMap ? "damage" : "off")) === expected.encoder.roiMode &&
    (encoder.resourceMode ?? "per-frame") === expected.encoder.resourceMode &&
    (encoder.asyncDepth ?? 0) === expected.encoder.asyncDepth &&
    (encoder.rateControlMode ?? "cbr") === (expected.encoder.rateControlMode ?? "cbr") &&
    encoder.targetQuality === expected.encoder.targetQuality &&
    encoder.peakBitrateMultiplier === expected.encoder.peakBitrateMultiplier;
  return encoder.temporalAq === expected.encoder.temporalAq;
}

async function poll(operation, timeoutMs, intervalMs = 100) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const value = await operation();
    if (value) return value;
    await sleep(intervalMs);
  }
  return null;
}

function sequenceReached(current, target) {
  if (!Number.isInteger(current)) return false;
  const delta = (current - target) & 0xffff;
  return delta === 0 || delta < 0x8000;
}

function timestampReached(current, target) {
  if (!Number.isInteger(current)) return false;
  const delta = (current - target) >>> 0;
  return delta < 0x80000000;
}

async function captureViewport(page, path, clip = { x: 0, y: 0, width: 1920, height: 1080, scale: 1 }) {
  const result = await page.call("Page.captureScreenshot", {
    format: "png", fromSurface: true, captureBeyondViewport: false,
    clip,
  });
  writeFileSync(path, Buffer.from(result.data, "base64"));
}

const RETRYABLE_ATOMIC_CAPTURE_ERRORS = new Set([
  "frame-identity-unavailable",
  "frame-identity-mismatch",
  "quality-frame-sequence-unavailable",
  "quality-frame-before-request",
  "quality-frame-sequence-mismatch",
  "quality-frame-not-delta",
  "quality-roi-map-missing",
  "quality-roi-coverage-invalid",
  "quality-hybrid-tile-missing",
  "quality-hybrid-motion-overlay",
]);

async function captureReceiverSnapshot(page, path) {
  const captured = await page.evaluate(`(() => {
    const diagnostics = window.__bubbleplayDiagnostics;
    const frame = diagnostics?.captureFrame?.() ?? null;
    if (!frame?.diagnostics) return frame;
    const continuity = diagnostics?.snapshot?.() ?? null;
    return {
      ...frame,
      diagnostics: continuity
        ? { ...continuity, ...frame.diagnostics, frameTrace: frame.diagnostics.frameTrace }
        : frame.diagnostics,
    };
  })()`);
  const prefix = "data:image/png;base64,";
  if (!captured?.pngDataUrl?.startsWith(prefix) || !captured?.diagnostics) {
    throw new StreamLabUnavailable("receiver-frame-capture-unavailable",
      "The receiver could not atomically capture its visible WSS frame and diagnostics.");
  }
  writeFileSync(path, Buffer.from(captured.pngDataUrl.slice(prefix.length), "base64"));
  return captured.diagnostics;
}

export async function captureReceiverFrame(page, path, options = {}) {
  if (!options.source) return captureReceiverSnapshot(page, path);
  const phase = options.phase ?? "quality-capture";
  const timeoutMs = Number(options.timeoutMs ?? 5_000);
  if (!Number.isFinite(timeoutMs) || timeoutMs <= 0)
    throw new RangeError("quality capture timeout must be positive");
  const alignmentOptions = {
    ...(options.alignmentOptions ?? {}),
    // The PNG is always the current canvas. A historical same-identity delta
    // trace may prove admission while polling, but cannot describe these pixels.
    allowHistoricalDelta: false,
  };
  const started = performance.now();
  let lastDiagnostics = null;
  let lastAlignmentFailure = null;
  do {
    lastDiagnostics = await captureReceiverSnapshot(page, path);
    try {
      exactFrameAlignment(options.source, lastDiagnostics, phase, alignmentOptions);
      return lastDiagnostics;
    } catch (error) {
      if (!RETRYABLE_ATOMIC_CAPTURE_ERRORS.has(error?.code)) throw error;
      lastAlignmentFailure = summarizeAlignmentFailure(error);
    }
    if (performance.now() - started < timeoutMs) await sleep(50);
  } while (performance.now() - started < timeoutMs);
  throw new StreamLabUnavailable(
    "quality-capture-alignment-timeout",
    "The atomic receiver capture did not reach an exact frame satisfying the quality activity gate.",
    {
      phase,
      source: options.source,
      lastAlignmentFailure,
      frameTrace: lastDiagnostics?.worker?.frameTrace ?? lastDiagnostics?.frameTrace ?? null,
      hybrid: lastDiagnostics?.wssMedia?.hybrid ?? lastDiagnostics?.worker?.hybrid ??
        lastDiagnostics?.hybrid ?? null,
    },
  );
}

function captureDesktopRegion(display, path) {
  const result = spawnSync("ffmpeg", ["-hide_banner", "-loglevel", "error", "-y", "-f", "gdigrab",
    "-offset_x", String(display.x), "-offset_y", String(display.y), "-video_size", `${display.width}x${display.height}`,
    "-i", "desktop", "-frames:v", "1", path], { encoding: "utf8", windowsHide: true, timeout: 10_000 });
  if (result.status !== 0) throw new Error(`OS display capture failed: ${String(result.stderr || "unknown").trim()}`);
}

function placeNativeWindow(pid, display) {
  const signature = "using System; using System.Runtime.InteropServices; public static class BubblePlayWindow {" +
    "[DllImport(\"user32.dll\")] public static extern bool ShowWindow(IntPtr hWnd,int nCmdShow);" +
    "[DllImport(\"user32.dll\",SetLastError=true)] public static extern bool SetWindowPos(IntPtr hWnd,IntPtr after,int x,int y,int cx,int cy,uint flags);}";
  const script = [
    `Add-Type -TypeDefinition '${signature.replaceAll("'", "''")}'`,
    `$p=Get-Process -Id ${pid} -ErrorAction Stop`,
    "for($i=0;$i -lt 50 -and $p.MainWindowHandle -eq 0;$i++){Start-Sleep -Milliseconds 100;$p.Refresh()}",
    "if($p.MainWindowHandle -eq 0){throw 'Chrome main window handle unavailable'}",
    "[void][BubblePlayWindow]::ShowWindow($p.MainWindowHandle,9)",
    `if(-not [BubblePlayWindow]::SetWindowPos($p.MainWindowHandle,[IntPtr]::Zero,${display.x},${display.y},${display.width},${display.height},0x0040)){throw 'SetWindowPos failed'}`,
  ].join("; ");
  const result = spawnSync("powershell.exe", ["-NoProfile", "-NonInteractive", "-Command", script],
    { encoding: "utf8", windowsHide: true, timeout: 10_000 });
  if (result.status !== 0) throw new StreamLabUnavailable("source-native-placement",
    "Windows could not place the deterministic source on the transient display.", String(result.stderr || result.stdout).trim());
}

function activateProcessWindow(pid) {
  const script = `$shell=New-Object -ComObject WScript.Shell; if(-not $shell.AppActivate(${pid})){throw 'AppActivate failed'}`;
  const result = spawnSync("powershell.exe", ["-NoProfile", "-NonInteractive", "-Command", script],
    { encoding: "utf8", windowsHide: true, timeout: 5_000 });
  if (result.status !== 0) throw new StreamLabUnavailable("source-window-activation",
    "Windows could not activate the isolated source before the remote-input probe.", String(result.stderr || result.stdout).trim());
}

function scoreImagePair(referencePath, decodedPath, outputBase) {
  const ffmpeg = runFfmpegQuality(referencePath, decodedPath, `${outputBase}-ffmpeg.json`);
  const referenceOcr = runTesseractOcr(referencePath, `${outputBase}-reference-ocr`, "");
  const decodedOcr = runTesseractOcr(decodedPath, `${outputBase}-decoded-ocr`, referenceOcr.observed);
  const reference = readImageLuma(referencePath);
  const decoded = readImageLuma(decodedPath);
  if (reference.width !== decoded.width || reference.height !== decoded.height)
    throw new StreamLabUnavailable("sample-geometry", "Reference and decoded samples have different dimensions.");
  const regional = scoreLumaTiles(reference.luma, decoded.luma, reference.width, reference.height, { columns: 8, rows: 4 });
  const edgeFidelity = compareEdgeFidelity(reference.luma, decoded.luma, reference.width, reference.height);
  const colorError = compareColorError(referencePath, decodedPath, reference.width, reference.height);
  return {
    vmaf: ffmpeg.vmaf, ssim: ffmpeg.ssim, psnr: ffmpeg.psnr, blur: ffmpeg.blur, block: ffmpeg.block,
    ocrCer: characterErrorRate(referenceOcr.observed, decodedOcr.observed),
    referenceOcr: referenceOcr.observed, decodedOcr: decodedOcr.observed,
    tileSsim: regional,
    edgeFidelity,
    colorError,
    blockArtifactSeverity: ffmpeg.block,
    artifactPaths: [...Object.values(ffmpeg.files ?? {}), `${outputBase}-reference-ocr.txt`, `${outputBase}-decoded-ocr.txt`],
  };
}

function scoreForTelemetry(score) {
  return { vmaf: score.vmaf, ssim: score.ssim, psnr: score.psnr, blur: score.blur,
    block: score.block, blockArtifactSeverity: score.blockArtifactSeverity, edgeFidelity: score.edgeFidelity,
    colorError: score.colorError, ocrCer: score.ocrCer, worstTileSsim: score.tileSsim?.worst ?? null };
}

function compareEdgeFidelity(reference, decoded, width, height) {
  let difference = 0, magnitude = 0;
  for (let y = 1; y < height - 1; y += 2) for (let x = 1; x < width - 1; x += 2) {
    const index = y * width + x;
    const refGradient = Math.abs(reference[index + 1] - reference[index - 1]) +
      Math.abs(reference[index + width] - reference[index - width]);
    const decGradient = Math.abs(decoded[index + 1] - decoded[index - 1]) +
      Math.abs(decoded[index + width] - decoded[index - width]);
    difference += Math.abs(refGradient - decGradient);
    magnitude += Math.max(1, refGradient, decGradient);
  }
  return magnitude ? Math.max(0, Math.min(1, 1 - difference / magnitude)) : 1;
}

function compareColorError(referencePath, decodedPath, width, height) {
  const reference = readImageRgb(referencePath, width, height);
  const decoded = readImageRgb(decodedPath, width, height);
  let error = 0;
  for (let index = 0; index < reference.length; index += 3) {
    error += (Math.abs(reference[index] - decoded[index]) + Math.abs(reference[index + 1] - decoded[index + 1]) +
      Math.abs(reference[index + 2] - decoded[index + 2])) / 3;
  }
  return error / (reference.length / 3);
}

function readImageRgb(path, width, height) {
  const result = spawnSync("ffmpeg", ["-hide_banner", "-loglevel", "error", "-i", path,
    "-frames:v", "1", "-pix_fmt", "rgb24", "-f", "rawvideo", "-"],
  { encoding: null, windowsHide: true, timeout: 30_000, maxBuffer: Math.max(32 * 1024 * 1024, width * height * 3 + 1024) });
  if (result.status !== 0 || result.stdout.length !== width * height * 3)
    throw new StreamLabUnavailable("sample-color-decode", "FFmpeg could not decode an RGB parity sample.",
      { path, status: result.status, bytes: result.stdout?.length ?? 0, expectedBytes: width * height * 3 });
  return result.stdout;
}

function summarizeQuality(scores, temporal = null) {
  const numeric = (field) => scores.map(score => field(score)).filter(Number.isFinite);
  return {
    qualitySamples: scores.length,
    ocrCer: median(numeric(score => score.ocrCer)),
    worstTileSsim: Math.min(...numeric(score => score.tileSsim?.worst)),
    p5TileSsim: percentile(numeric(score => score.tileSsim?.p5), 0.05),
    vmaf: median(numeric(score => score.vmaf)),
    ssim: median(numeric(score => score.ssim)),
    psnr: median(numeric(score => score.psnr)),
    blur: median(numeric(score => score.blur)),
    block: median(numeric(score => score.block)),
    temporalUnchangedError: temporal?.mean ?? null,
    temporalWorstTileError: temporal?.worstTile ?? null,
  };
}

function sampleSystemTelemetry() {
  const processResult = spawnSync("powershell.exe", ["-NoProfile", "-NonInteractive", "-Command",
    "$p=Get-Process -Name bubbleplay-host -ErrorAction SilentlyContinue | Select-Object -First 1; if($p){[pscustomobject]@{cpuSeconds=$p.CPU;privateMemoryBytes=$p.PrivateMemorySize64;workingSetBytes=$p.WorkingSet64}|ConvertTo-Json -Compress}"],
  { encoding: "utf8", windowsHide: true, timeout: 5_000 });
  let process = {};
  try { process = JSON.parse(processResult.stdout || "{}"); } catch {}
  const gpuResult = spawnSync("nvidia-smi", ["--query-gpu=utilization.gpu,utilization.encoder,memory.used,power.draw", "--format=csv,noheader,nounits"],
    { encoding: "utf8", windowsHide: true, timeout: 5_000 });
  const [gpuUtilizationPct, encoderUtilizationPct, vramUsedMb, powerWatts] = String(gpuResult.stdout || "").trim().split(",").map(Number);
  return { ...process, gpuUtilizationPct, encoderUtilizationPct, vramUsedMb, powerWatts };
}

function safeName(value) {
  return String(value).replace(/[^A-Za-z0-9._-]/g, "_").slice(0, 96);
}

export async function runLiveManual(options) {
  if (!options?.candidate) throw new TypeError("manual use requires an explicit candidate profile");
  const session = options.session ?? new LiveStreamLabSession(options);
  try {
    await session.start("wss", { mode: "manual" });
    const snapshot = await session.applyForManualUse(options.candidate, null, "wss");
    const report = {
      schemaVersion: 1,
      result: "running",
      mode: "manual",
      runId: options.runId,
      candidateId: options.candidate.id,
      experimentId: options.candidate.experimentId,
      hybridMode: options.candidate.hybridMode ?? "off",
      route: snapshot?.route ?? null,
      decoder: snapshot?.dec ?? null,
      interactionAutomation: false,
      message: "The manual candidate session is ready. Close it with Ctrl+C when finished.",
    };
    options.store?.writeMetrics?.(report);
    options.onReady?.(report);
    process.stderr.write("Stream Lab manual use is ready; the receiver will remain open with no automated pointer or viewport actions. Press Ctrl+C to stop.\n");
    await (options.waitUntilStopped ?? (() => new Promise(() => {})))();
    return report;
  } finally {
    await session.stop();
  }
}

export async function runLiveBaseline(options) {
  const candidate = options.candidate ?? {
    id: "baseline", preset: "p3", tuning: "ultra-low-latency", multipass: "disabled",
    vbvFrames: 1, aqStrength: 8, temporalAq: false, bitrateMbps: 6,
  };
  const session = new LiveStreamLabSession(options);
  const repetitions = [];
  try {
    await session.start("wss");
    for (let repetition = 0; repetition < (options.repetitions ?? 3); repetition++) {
      const bandwidthMbps = options.bandwidthMbps === undefined ? null : options.bandwidthMbps;
      const effective = constrainedConfiguration(candidate, bandwidthMbps, "wss");
      const stabilized = await stabilizeEncoderForCap(session, effective, bandwidthMbps, options);
      const performance = await session.collectPerformance(options.performanceSeconds ?? 15, {
        scene: options.performanceScene,
        actionProbe: options.actionProbe,
      });
      const perfMetrics = summarizePerformance(performance.samples, {
        transport: "wss", firstFrameMs: session.lastFirstFrameMs,
        actionToPhotonSamples: performance.actionToPhotonSamples,
        bandwidthMbps, encoderBitrateMbps: stabilized.stabilizedEncoderBitrateMbps,
        qosPolicy: session.qosPolicy,
      });
      assertShapingValid(perfMetrics, bandwidthMbps, session.qosPolicy, stabilized);
      const quality = await session.collectQuality(
        `${candidate.id}-r${repetition + 1}`,
        options.sceneIds,
        {
          maxSamples: options.qualitySamples ?? 12,
          deltaMode: options.deltaQuality === true ||
            ["png-static-roi", "png-quiescent-detail", "hevc-quiescent-tiles"].includes(
              effective.hybridMode),
          deltaTriggerFrames: options.deltaTriggerFrames,
          requireRoiActivity: options.deltaQuality === true &&
            (effective.roiMode ?? (effective.dirtyQpMap ? "damage" : "off")) !== "off",
          requireHybridActivity: effective.hybridMode !== "off",
          hybridMode: effective.hybridMode,
          hybridCacheMode: effective.hybridCacheMode,
        },
      );
      const quiescentInteractions = ["png-quiescent-detail", "hevc-quiescent-tiles"].includes(
        effective.hybridMode)
        ? shouldRunQuiescentInteractionTrials(effective.hybridMode, options.actionProbe)
          ? await session.collectQuiescentInteractionTrials()
          : { skipped: true, reason: "input-automation-disabled" }
        : null;
      repetitions.push({ id: `${candidate.id}-r${repetition + 1}`,
        ...stabilized, ...combinePerformanceAndQuality(perfMetrics, quality),
        actionProbe: performance.actionProbe,
        ...(quiescentInteractions ? { quiescentInteractions } : {}) });
    }
    return aggregateRepetitions(repetitions, candidate);
  } finally { await session.stop(); }
}

export async function applyWithFirstFrameRetry(session, effective, bandwidthMbps) {
  try { return await session.apply(effective, bandwidthMbps, "wss"); }
  catch (error) {
    if (error?.code !== "first-frame-timeout") throw error;
    session.store?.appendTelemetry?.({ phase: "first-frame-retry", at: new Date().toISOString() });
    await session.restartTransport("wss", session.receiverOptions);
    return session.apply(effective, bandwidthMbps, "wss");
  }
}

export async function runLiveOptimize(options) {
  const session = new LiveStreamLabSession(options);
  const completedUnits = options.resumeState?.units ?? {};
  const checkpointContext = { completedUnits, onCheckpoint: options.onCheckpoint,
    activitySnapshot: options.activitySnapshot };
  const checkpoint = (unitId, operation, reconcile = null) =>
    runCheckpointedUnit(checkpointContext, unitId, reconcile, operation);
  const baselineConfiguration = {
    schemaVersion: 2, implementationVariant: "production", experimentId: "production-baseline",
    preset: "p3", tuning: "ultra-low-latency", multipass: "disabled", aqMode: "spatial", aqStrength: 8,
    vbvFrames: 1, lookaheadFrames: 0, weightedPrediction: false, gopSeconds: 2, bitrateMbps: 6,
  };
  const bandwidths = [null, 2, 4, 8, 12];
  const candidates = generatePairwiseEncoderConfigurations({ mode: "work", count: 96 });
  const allRuns = [];
  try {
    await session.start("wss");
    const baselineRuns = [];
    for (const bandwidthMbps of bandwidths) for (let repetition = 1; repetition <= 3; repetition++) {
      const label = bandwidthMbps ?? "unshaped";
      const configuration = constrainedConfiguration(baselineConfiguration, bandwidthMbps, "wss");
      baselineRuns.push(await checkpoint(`opt:baseline:${label}:r${repetition}`, () =>
        measureOptimizationCandidate(session, { ...configuration, id: `production-${label}-r${repetition}` },
          bandwidthMbps, options, `baseline-${label}-r${repetition}`)));
    }
    allRuns.push(...baselineRuns);
    const baselines = new Map(bandwidths.map(bandwidthMbps => [bandwidthKey(bandwidthMbps),
      aggregateRepetitions(baselineRuns.filter(run => run.bandwidthMbps === bandwidthMbps),
        { ...baselineConfiguration, id: `production-${bandwidthMbps ?? "unshaped"}` })]));

    const screenRuns = [];
    for (const bandwidthMbps of [null, 4]) for (const candidate of candidates) {
      const configuration = constrainedConfiguration(candidate, bandwidthMbps, "wss");
      const unitId = `opt:screen:${candidate.experimentId}:${bandwidthMbps ?? "unshaped"}`;
      const wasCompleted = Object.hasOwn(completedUnits, unitId);
      const run = await checkpoint(unitId, () => measureOptimizationScreenCandidate(
        session, configuration, bandwidthMbps, options, unitId));
      if (shouldRecoverRejectedCandidate(run, wasCompleted))
        await session.recoverAfterRejectedCandidate(baselineConfiguration);
      const baseline = baselines.get(bandwidthKey(bandwidthMbps));
      screenRuns.push({ ...run, comparison: run.result === "rejected"
        ? { accepted: false, reasons: [run.rejection.code] }
        : evaluateOptimizationRun(baseline, run, options) });
    }
    allRuns.push(...screenRuns);
    const screened = candidates.map(candidate => {
      const runs = screenRuns.filter(run => run.experimentId === candidate.experimentId);
      const rejections = runs.filter(run => run.result === "rejected");
      if (rejections.length) return { id: candidate.experimentId, configuration: candidate,
        result: "rejected", rejections: rejections.map(run => run.rejection), accepted: false };
      return { ...aggregateRepetitions(runs, { ...candidate, id: candidate.experimentId }),
        comparisons: runs.map(run => run.comparison), accepted: runs.length === 2 && runs.every(run => run.comparison.accepted) };
    }).filter(candidate => candidate.accepted);
    let top16 = rankOptimizationCandidates(selectAndFillFrontier(screened, 16)).slice(0, 16);
    let refinement = null;
    if (!top16.length) {
      const refinementCandidates = generateArtifactRefinementConfigurations();
      const refinementBaselineRuns = [];
      for (const bandwidthMbps of [null, 4]) for (let repetition = 1; repetition <= 3; repetition++) {
        const label = bandwidthMbps ?? "unshaped";
        const configuration = constrainedConfiguration(baselineConfiguration, bandwidthMbps, "wss");
        refinementBaselineRuns.push(await checkpoint(`opt:refine-baseline:${label}:r${repetition}`, () =>
          measureOptimizationCandidate(session,
            { ...configuration, experimentId: `refine-production-${label}-r${repetition}` },
            bandwidthMbps, options, `refine-baseline-${label}-r${repetition}`)));
      }
      allRuns.push(...refinementBaselineRuns);
      const refinementBaselines = new Map([null, 4].map(bandwidthMbps => [bandwidthKey(bandwidthMbps),
        aggregateRepetitions(refinementBaselineRuns.filter(run => run.bandwidthMbps === bandwidthMbps),
          { ...baselineConfiguration, id: `refine-production-${bandwidthMbps ?? "unshaped"}` })]));
      const refinementRuns = [];
      for (const bandwidthMbps of [null, 4]) for (const candidate of refinementCandidates) {
        const configuration = constrainedConfiguration(candidate, bandwidthMbps, "wss");
        const unitId = `opt:refine:${candidate.experimentId}:${bandwidthMbps ?? "unshaped"}`;
        const wasCompleted = Object.hasOwn(completedUnits, unitId);
        const run = await checkpoint(unitId, () => measureOptimizationScreenCandidate(
          session, configuration, bandwidthMbps, options, unitId));
        if (shouldRecoverRejectedCandidate(run, wasCompleted))
          await session.recoverAfterRejectedCandidate(baselineConfiguration);
        const baseline = refinementBaselines.get(bandwidthKey(bandwidthMbps));
        refinementRuns.push({ ...run, comparison: run.result === "rejected"
          ? { accepted: false, reasons: [run.rejection.code] }
          : evaluateOptimizationRun(baseline, run, options) });
      }
      allRuns.push(...refinementRuns);
      const refined = refinementCandidates.map(candidate => {
        const runs = refinementRuns.filter(run => run.experimentId === candidate.experimentId);
        const rejections = runs.filter(run => run.result === "rejected");
        if (rejections.length) return { id: candidate.experimentId, configuration: candidate,
          result: "rejected", rejections: rejections.map(run => run.rejection), accepted: false };
        return { ...aggregateRepetitions(runs, { ...candidate, id: candidate.experimentId }),
          comparisons: runs.map(run => run.comparison),
          accepted: runs.length === 2 && runs.every(run => run.comparison.accepted) };
      });
      refinement = { baselines: [...refinementBaselines.entries()].map(([bandwidth, metrics]) => ({ bandwidth, ...metrics })),
        candidates: refined, runs: refinementRuns };
      top16 = rankOptimizationCandidates(selectAndFillFrontier(refined.filter(candidate => candidate.accepted), 16)).slice(0, 16);
    }
    if (!top16.length) throw new StreamLabUnavailable("optimization-refinement-empty",
      "No encoder candidate passed fresh reference-aware refinement on both routes.", refinement ?? screenRuns);

    const validationRuns = [];
    for (const bandwidthMbps of [2, 4, 8]) for (const candidate of top16) {
      const configuration = constrainedConfiguration(candidate.configuration, bandwidthMbps, "wss");
      const unitId = `opt:validate:${candidate.id}:${bandwidthMbps}`;
      const run = await checkpoint(unitId, () => measureOptimizationCandidate(session, configuration, bandwidthMbps, options, unitId));
      const baseline = baselines.get(bandwidthKey(bandwidthMbps));
      validationRuns.push({ ...run, comparison: evaluateOptimizationRun(baseline, run, options) });
    }
    allRuns.push(...validationRuns);
    const validated = top16.map(candidate => {
      const runs = validationRuns.filter(run => run.experimentId === candidate.configuration.experimentId);
      return { ...aggregateRepetitions(runs, candidate.configuration), id: candidate.id, comparisons: runs.map(run => run.comparison),
        accepted: runs.length === 3 && runs.every(run => run.comparison.accepted) };
    }).filter(candidate => candidate.accepted);
    const top4 = rankOptimizationCandidates(selectAndFillFrontier(validated, 4)).slice(0, 4);
    if (!top4.length) throw new StreamLabUnavailable("optimization-validation-empty", "No screened candidate survived shaped validation.", validationRuns);

    const abbaRuns = [];
    const abba = buildAbbaOrder("production", "candidate", options.abbaRepetitions ?? 1);
    for (const bandwidthMbps of [2, 4, 8, 12]) for (const candidate of top4) for (const position of abba) {
      const selected = position.variant === "candidate" ? candidate.configuration : {
        ...baselineConfiguration, bitrateMbps: candidate.configuration.bitrateMbps,
        experimentId: `production-control-${candidate.id}`,
      };
      const configuration = constrainedConfiguration(selected, bandwidthMbps, "wss");
      const unitId = `opt:abba:${candidate.id}:${bandwidthMbps}:r${position.repetition}:p${position.position}:${position.variant}`;
      const run = await checkpoint(unitId, () => measureOptimizationCandidate(session, configuration, bandwidthMbps, options, unitId));
      abbaRuns.push({ ...run, candidateId: candidate.id, abbaVariant: position.variant,
        abbaRepetition: position.repetition, abbaPosition: position.position });
    }
    allRuns.push(...abbaRuns);
    const finalists = top4.map(candidate => {
      const candidateRuns = abbaRuns.filter(run => run.candidateId === candidate.id && run.abbaVariant === "candidate");
      const productionRuns = abbaRuns.filter(run => run.candidateId === candidate.id && run.abbaVariant === "production");
      const candidateAggregate = aggregateRepetitions(candidateRuns, candidate.configuration);
      const productionAggregate = aggregateRepetitions(productionRuns, { ...baselineConfiguration, id: `production-control-${candidate.id}` });
      const comparison = evaluateOptimizationRun(productionAggregate, candidateAggregate, options);
      return { ...candidateAggregate, id: candidate.id, comparison, accepted: comparison.accepted,
        abba: { order: abba.map(position => position.variant), candidateRuns, productionRuns } };
    });
    const frontier = rankOptimizationCandidates(selectParetoFrontier(finalists.filter(finalist => finalist.accepted),
      { noise: combinedOptimizationNoise([...baselines.values()]) }));
    if (!frontier.length) throw new StreamLabUnavailable("optimization-frontier-empty", "ABBA validation rejected every candidate.", finalists);
    let gates = options.gates ?? { qualityFloors: {}, ceilings: {} };
    for (const champion of frontier) gates = ratchetGates(gates, champion);
    return {
      result: "complete", strategy: "pairwise-parity-abba", objective: "source-quality-primary",
      acceptanceRule: "material-quality-improvement-without-quality-regression-and-with-performance-gates",
      baselineMatrix: [...baselines.entries()].map(([bandwidth, metrics]) => ({ bandwidth, ...metrics })),
      screened, refinement, top16, validated, top4, finalists, frontier, ratchetedGates: gates, runs: allRuns,
    };
  } finally { await session.stop(); }
}

export async function measureOptimizationCandidate(session, configuration, bandwidthMbps, options, artifactId = configuration.experimentId) {
  const stabilized = await stabilizeEncoderForCap(session, configuration, bandwidthMbps, options);
  const performance = await session.collectPerformance(options.optimizationPerformanceSeconds ?? 5);
  const perf = summarizePerformance(performance.samples, {
    transport: "wss", firstFrameMs: session.lastFirstFrameMs,
    actionToPhotonSamples: performance.actionToPhotonSamples, bandwidthMbps,
    encoderBitrateMbps: stabilized.stabilizedEncoderBitrateMbps, qosPolicy: session.qosPolicy,
  });
  assertShapingValid(perf, bandwidthMbps, session.qosPolicy, stabilized);
  const quality = await session.collectQuality(artifactId,
    options.optimizationScenes ?? ["ui-text", "gradients", "motion-60", "localized-change", "source-code", "chrome-text", "chrome-canvas", "generated-video"],
    { maxSamples: options.optimizationQualitySamples ?? 12, retainWorst: 10 });
  return {
    id: configuration.experimentId, experimentId: configuration.experimentId,
    implementationVariant: configuration.implementationVariant, configuration, ...stabilized,
    bandwidthMbps, ...combinePerformanceAndQuality(perf, quality),
  };
}

export async function measureOptimizationScreenCandidate(session, configuration, bandwidthMbps, options, artifactId) {
  try {
    return await measureOptimizationCandidate(session, configuration, bandwidthMbps, options, artifactId);
  } catch (error) {
    const rejection = candidateStartupRejection(error, configuration, bandwidthMbps) ??
      candidateRuntimeRejection(error, configuration, bandwidthMbps);
    if (!rejection) throw error;
    session.store?.appendTelemetry?.({ phase: "encoder-candidate-rejected", at: new Date().toISOString(),
      experimentId: configuration.experimentId, bandwidthMbps, rejection: rejection.rejection });
    return rejection;
  }
}

export async function stabilizeEncoderForCap(session, configuration, bandwidthMbps, options = {}, behavior = {}) {
  const requestedBitrateMbps = Number(configuration.requestedBitrateMbps ?? configuration.bitrateMbps ?? 6);
  if (!Number.isFinite(bandwidthMbps)) {
    const effectiveConfiguration = { ...configuration, requestedBitrateMbps, bitrateMbps: requestedBitrateMbps };
    await session.apply(effectiveConfiguration, null, "wss");
    return { requestedBitrateMbps, initialEncoderBitrateMbps: requestedBitrateMbps,
      stabilizedEncoderBitrateMbps: requestedBitrateMbps, stabilization: [], effectiveConfiguration };
  }
  const targetKey = JSON.stringify({ bandwidthMbps,
    streamMode: configuration.streamMode ?? "work", fps: configuration.fps ?? 30,
    preset: configuration.preset, tuning: configuration.tuning ?? "ultra-low-latency",
    multipass: configuration.multipass, aqMode: configuration.aqMode ??
      (configuration.temporalAq ? "temporal" : "none"), aqStrength: configuration.aqStrength,
    vbvFrames: configuration.vbvFrames, lookaheadFrames: configuration.lookaheadFrames ??
    configuration.lookaheadDepth ?? 0, weightedPrediction: configuration.weightedPrediction ?? false,
    dirtyQpMap: configuration.dirtyQpMap ?? false, main10: configuration.main10 ?? false,
    roiMode: configuration.roiMode ?? (configuration.dirtyQpMap ? "damage" : "off"),
    resourceMode: configuration.resourceMode ?? "per-frame",
    asyncDepth: configuration.asyncDepth ?? 0, hybridMode: configuration.hybridMode ?? "off",
    hybridCacheMode: configuration.hybridCacheMode ?? "suppress-reactivate",
    gopSeconds: configuration.gopSeconds ?? 2, requestedBitrateMbps });
  session.stabilizedTargets ??= new Map();
  const calculatedInitialEncoderBitrateMbps = initialEncoderTargetMbps(requestedBitrateMbps, bandwidthMbps);
  const priorSafeTarget = Number(session.stabilizedTargets.get(targetKey));
  const hasPriorSafeTarget = Number.isFinite(priorSafeTarget);
  const initialEncoderBitrateMbps = hasPriorSafeTarget
    ? priorSafeTarget
    : calculatedInitialEncoderBitrateMbps;
  let target = initialEncoderBitrateMbps;
  const stabilization = [];
  const complete = (effectiveConfiguration) => {
    session.stabilizedTargets.set(targetKey, target);
    return { requestedBitrateMbps, calculatedInitialEncoderBitrateMbps, initialEncoderBitrateMbps,
      stabilizedEncoderBitrateMbps: target, stabilization, effectiveConfiguration };
  };
  for (let correction = 0; correction <= 2; correction++) {
    const effectiveConfiguration = { ...configuration, requestedBitrateMbps,
      totalMediaCapMbps: bandwidthMbps, bitrateMbps: target };
    if (behavior.firstFrameRetry) await applyWithFirstFrameRetry(session, effectiveConfiguration, bandwidthMbps);
    else await session.apply(effectiveConfiguration, bandwidthMbps, "wss");
    const measured = await session.collectPerformance(options.stabilizationSeconds ?? 5, { actionProbe: false });
    const metrics = summarizePerformance(measured.samples, { transport: "wss", firstFrameMs: session.lastFirstFrameMs,
      bandwidthMbps, encoderBitrateMbps: target, qosPolicy: session.qosPolicy });
    const reasons = shapingFailureReasons(metrics, bandwidthMbps, session.qosPolicy);
    stabilization.push({ correction, effectiveEncoderBitrateMbps: target, metrics, reasons });
    const nonCorrectable = reasons.filter(reason => reason !== "offered-cap-overshoot");
    if (nonCorrectable.length) assertShapingValid(metrics, bandwidthMbps, session.qosPolicy,
      { requestedBitrateMbps, initialEncoderBitrateMbps, stabilizedEncoderBitrateMbps: target, stabilization });
    const offered = metrics.offeredWireBitrateMbps;
    const underTarget = !hasPriorSafeTarget && Number.isFinite(offered) &&
      offered < bandwidthMbps * 0.85 && target < requestedBitrateMbps;
    const overTarget = reasons.includes("offered-cap-overshoot");
    if (!reasons.includes("offered-cap-overshoot") && !underTarget && !overTarget)
      return complete(effectiveConfiguration);
    if (correction === 2) {
      if (reasons.length === 0) return complete(effectiveConfiguration);
      break;
    }
    const next = Math.min(requestedBitrateMbps, nextStabilizedBitrateMbps(target, offered, bandwidthMbps));
    if (next === target && reasons.length === 0)
      return complete(effectiveConfiguration);
    target = next;
  }
  throw new StreamLabUnavailable("bandwidth-shaping-unverified",
    "Encoder stabilization could not keep offered WSS traffic below the run-owned total-media cap.",
    { requestedBitrateMbps, initialEncoderBitrateMbps, stabilizedEncoderBitrateMbps: target,
      bandwidthMbps, qosPolicy: session.qosPolicy, stabilization });
}

function assertShapingValid(metrics, bandwidthMbps, qosPolicy, stabilization = null) {
  const reasons = shapingFailureReasons(metrics, bandwidthMbps, qosPolicy);
  if (reasons.length) {
    const shapingFailed = reasons.some(reason => BANDWIDTH_SHAPING_REASONS.has(reason));
    throw new StreamLabUnavailable(
      shapingFailed ? "bandwidth-shaping-unverified" : "performance-hard-gate-failed",
      shapingFailed
        ? "The shaped performance pass could not verify the total-media cap."
        : "The shaped performance pass failed a runtime hard gate before quality capture.",
      { bandwidthMbps, qosPolicy, metrics, reasons, stabilization },
    );
  }
}

export function evaluateOptimizationRun(baseline, candidate, options) {
  const parity = evaluateParetoAcceptance(baseline, candidate,
    { noise: baseline?.noise ?? {}, gates: options.gates, qualityPrimary: true });
  const hardGates = [];
  const roiMode = candidate?.configuration?.roiMode ??
    candidate?.configuration?.encoder?.roiMode ??
    (candidate?.configuration?.dirtyQpMap === true ||
      candidate?.configuration?.encoder?.dirtyQpMap === true ? "damage" : "off");
  const hybridMode = candidate?.configuration?.hybridMode ?? "off";
  if (roiMode !== "off" && !(Number(candidate.qpMapFrames) > 0))
    hardGates.push("roi-activity-missing");
  else if (roiMode !== "off" &&
      Number(candidate?.roiActivity?.mappedFrames ?? candidate.qpMapFrames) <
        (["feedback", "feedback-headroom"].includes(roiMode) ? 20 : 30))
    hardGates.push("roi-activity-insufficient");
  if (roiMode !== "off" && Number(candidate.mappedQualitySamples) < 1)
    hardGates.push("roi-quality-activity-missing");
  if (roiMode === "damage") {
    const temporal = candidate?.roiActivity;
    if (temporal?.source !== "gpu-temporal" ||
        !(Number(temporal?.temporalFrames) > 0) ||
        !Number.isFinite(Number(temporal?.analysisP95Us)))
      hardGates.push("temporal-analysis-missing");
    else if (Number(temporal.analysisP95Us) > 1_500)
      hardGates.push("temporal-analysis-overhead");
  }
  if (["feedback", "feedback-headroom"].includes(roiMode)) {
    const feedback = candidate?.roiActivity;
    if (feedback?.source !== "gpu-temporal" || !(Number(feedback?.temporalFrames) > 0) ||
        !Number.isFinite(Number(feedback?.analysisP95Us)))
      hardGates.push("temporal-analysis-missing");
    else if (Number(feedback.analysisP95Us) > 1_500)
      hardGates.push("temporal-analysis-overhead");
    if (feedback?.reconstructionActive !== true)
      hardGates.push("feedback-reconstruction-inactive");
    if (!(Number(feedback?.feedbackAdjustedFrames) >= 20))
      hardGates.push("feedback-activity-insufficient");
    if (!(Number(feedback?.feedbackReadyFrames) > 0) ||
        Number(feedback?.lastFeedbackAgeFrames) !== 1)
      hardGates.push("feedback-exact-sequence-missing");
    const feedbackProcessingP95Us = Number(feedback?.feedbackSubmitP95Us) +
      Number(feedback?.feedbackReadbackP95Us);
    if (!Number.isFinite(feedbackProcessingP95Us))
      hardGates.push("feedback-processing-telemetry-missing");
    else if (feedbackProcessingP95Us > 2_000)
      hardGates.push("feedback-processing-overhead");
    if (roiMode === "feedback-headroom") {
      if (!(Number(feedback?.feedbackSpendHeadroomFrames) >= 20))
        hardGates.push("feedback-headroom-inactive");
      const occupancyP95Bps = Number(feedback?.feedbackWireOccupancyP95Bps);
      const occupancyMaxBps = Number(feedback?.feedbackWireOccupancyMaxBps);
      if (!Number.isFinite(occupancyP95Bps) || !Number.isFinite(occupancyMaxBps)) {
        hardGates.push("feedback-headroom-telemetry-missing");
      } else {
        const capBps = Number(candidate?.configuration?.totalMediaCapMbps ??
          candidate?.configuration?.bitrateMbps ?? 6) * 1_000_000;
        if (occupancyMaxBps > capBps)
          hardGates.push("feedback-headroom-cap-risk");
      }
    }
  }
  if (hybridMode !== "off" && Number(candidate.hybridQualitySamples) < 1)
    hardGates.push("hybrid-quality-activity-missing");
  if (hybridMode !== "off" && Number(candidate?.hybridActivity?.staleComposites) !== 0)
    hardGates.push("hybrid-stale-composite");
  if (["png-quiescent-detail", "hevc-quiescent-tiles"].includes(hybridMode)) {
    const hostHybrid = candidate?.hostHybridActivity;
    if (!(Number(hostHybrid?.frames) > 0))
      hardGates.push("quiescent-activity-missing");
    else if (Number(hostHybrid?.analysisP95Us) > 1_500)
      hardGates.push("quiescent-analysis-overhead");
    if (hostHybrid?.validationFrames !== undefined) {
      if (!(Number(hostHybrid.validationFrames) > 0) ||
          !Number.isFinite(Number(hostHybrid.validationCollectP95Us)))
        hardGates.push("quiescent-validation-telemetry-missing");
      else if (Number(hostHybrid.validationCollectP95Us) > 200)
        hardGates.push("quiescent-validation-collect-overhead");
      if (Number(hostHybrid.validationFailedFrames) > 0)
        hardGates.push("quiescent-validation-failed");
      const suppressions = Number(hostHybrid.suppressions);
      const resolved = Number(hostHybrid.reactivations) +
        Number(hostHybrid.permanentInvalidations);
      const validationAgeMax = Number(hostHybrid.deferredValidationAgeMaxFrames);
      const pendingAgeMax = Number(hostHybrid.deferredValidationPendingMaxAgeFrames);
      const boundedPendingReadback = Number(hostHybrid.deferredValidationPendingFrames) > 0 &&
        Number(hostHybrid.deferredValidationReadyFrames) > 0 &&
        Number.isFinite(validationAgeMax) && validationAgeMax <= 2 &&
        Number.isFinite(pendingAgeMax) && pendingAgeMax <= 1 &&
        Number.isFinite(suppressions) && suppressions > 0 &&
        Number.isFinite(resolved) && resolved + 4 >= suppressions &&
        Number(hostHybrid.staleReactivationRejects) === 0;
      if (Number(hostHybrid.validationUnavailableFrames) > 0 && !boundedPendingReadback)
        hardGates.push("quiescent-validation-unavailable");
      if ((Number.isFinite(validationAgeMax) && validationAgeMax > 2) ||
          (Number.isFinite(pendingAgeMax) && pendingAgeMax > 1))
        hardGates.push("quiescent-validation-age");
      if (Number.isFinite(suppressions) && Number.isFinite(resolved) &&
          suppressions > resolved + 4)
        hardGates.push("quiescent-validation-unresolved");
      if (Number(hostHybrid.staleReactivationRejects) > 0)
        hardGates.push("quiescent-stale-reactivation");
    }
    if (hybridMode === "hevc-quiescent-tiles") {
      if (!(Number(hostHybrid?.hevcTileEncoderOpens) > 0) ||
          !(Number(hostHybrid?.upserts) > 0) || !(Number(hostHybrid?.hevcTileBytes) > 0))
        hardGates.push("hevc-tile-host-activity-missing");
      if (Number(hostHybrid?.hevcTileEncoderFailures) > 0)
        hardGates.push("hevc-tile-encoder-failure");
      if (!(Number(candidate?.hybridActivity?.decodedTiles) > 0))
        hardGates.push("hevc-tile-decoder-activity-missing");
      if (Number(candidate?.hybridActivity?.decoderErrors) > 0)
        hardGates.push("hevc-tile-decoder-error");
      if (Number(candidate?.hybridActivity?.decoderPressureBypasses) > 0)
        hardGates.push("hevc-tile-decoder-pressure");
    }
    const presentation = candidate?.wssPresentation;
    if (presentation?.protocol !== 1) {
      hardGates.push("wss-presentation-telemetry-missing");
    } else {
      if (!(Number(presentation.immediateAttempts) > 0) ||
          Number(presentation.decodedFrames) !== Number(presentation.immediateAttempts))
        hardGates.push("wss-immediate-path-bypass");
      if (Number(presentation.immediatePresented) !== Number(presentation.immediateAttempts) ||
          Number(presentation.presentedFrames) !== Number(presentation.immediatePresented))
        hardGates.push("wss-immediate-presentation-loss");
      if (Number(presentation.rafEnqueued) > 0) hardGates.push("wss-raf-enqueue");
      if (Number(presentation.rafCoalesced) > 0) hardGates.push("wss-raf-coalescing");
      if (Number(presentation.rafLateDrops) > 0) hardGates.push("wss-raf-late-drop");
    }
  }
  if (!Number.isFinite(candidate.firstFrameMs) || candidate.firstFrameMs > 5_000) hardGates.push("first-frame-timeout");
  if (!Number.isFinite(candidate.presentedFps) || candidate.presentedFps < 27) hardGates.push("low-presented-fps");
  if (!Number.isFinite(candidate.latencyP95Ms) || candidate.latencyP95Ms > 500) hardGates.push("stream-latency");
  if ((candidate.freezes ?? 0) > 0) hardGates.push("freeze");
  if ((candidate.decoderResets ?? 0) > 0) hardGates.push("decoder-reset");
  if ((candidate.dependencyLosses ?? 0) > 0) hardGates.push("dependency-loss");
  if ((candidate.spatialTransitions ?? 0) > 0) hardGates.push("spatial-transition");
  if ((candidate.queueSlopePerMinute ?? 0) > 0) hardGates.push("queue-growth");
  const performanceContaminated = candidate?.performanceContaminated === true;
  const enforcedHardGates = performanceContaminated ? [] : hardGates;
  const advisoryHardGates = performanceContaminated ? hardGates : [];
  const reasons = [...new Set([...parity.reasons, ...enforcedHardGates])];
  return { ...parity, accepted: reasons.length === 0, reasons, hardGates: enforcedHardGates,
    advisoryHardGates, performanceValidation: performanceContaminated ? "possibly-inaccurate" : "verified" };
}

function selectAndFillFrontier(candidates, limit) {
  const frontier = selectParetoFrontier(candidates);
  const selected = [...frontier];
  for (const candidate of rankOptimizationCandidates(candidates)) {
    if (selected.length >= limit) break;
    if (!selected.includes(candidate)) selected.push(candidate);
  }
  return selected;
}

function rankOptimizationCandidates(candidates) {
  return [...candidates].sort((a, b) =>
    (b.vmafP5 ?? -Infinity) - (a.vmafP5 ?? -Infinity) ||
    (b.worstTileSsim ?? -Infinity) - (a.worstTileSsim ?? -Infinity) ||
    (b.edgeFidelity ?? -Infinity) - (a.edgeFidelity ?? -Infinity) ||
    (a.colorError ?? Infinity) - (b.colorError ?? Infinity) ||
    (a.latencyP95Ms ?? Infinity) - (b.latencyP95Ms ?? Infinity) || String(a.id).localeCompare(String(b.id)));
}

function combinedOptimizationNoise(baselines) {
  const fields = new Set(baselines.flatMap(baseline => Object.keys(baseline.noise ?? {})));
  return Object.fromEntries([...fields].map(field => [field, Math.max(...baselines.map(baseline => baseline.noise?.[field] ?? 0))]));
}

function bandwidthKey(value) { return value === null ? "unshaped" : String(value); }

function aggregateVerification(values) {
  if (values.some(value => value === false)) return false;
  return values.every(value => value === true) ? true : null;
}

export function aggregateExperimentRepetitions(repetitions, candidate) {
  const fields = ["firstFrameMs", "presentedFps", "encodeP95Ms", "latencyP95Ms", "actionToPhotonP95Ms",
    "sourceToPresentedP50Ms", "sourceToPresentedP95Ms", "receiveToPresentP50Ms", "receiveToPresentP95Ms",
    "encodedBitrateMbps", "wireBitrateMbps", "wireExpansion", "encodedBitrateCv", "wireBitrateCv",
    "frameGapP95Ms", "frameGapMaxMs", "hostCpuPct", "gpuUtilizationMeanPct", "gpuEncoderMeanPct",
    "privateMemoryPeakMb", "vramPeakMb", "gpuPowerMeanWatts", "memorySlopeMbPerMinute",
    "ocrCer", "worstTileSsim", "vmaf", "ssim", "psnr", "vmafMean", "vmafP5", "ssimMean", "ssimP5",
    "edgeFidelity", "colorError", "unchangedRegionError", "blockArtifactSeverity", "exactFrameRatio",
    "presentationCadenceP95Ms", "qpMapFrames", "qpMapChangedCtbs", "qpMapTotalCtbs"];
  const aggregate = { id: candidate.id ?? "baseline", configuration: candidate.configuration ?? candidate,
    result: "complete", transport: "wss", codec: "hevc", repetitions };
  aggregate.noise = {};
  const lowerIsWorse = new Set(["presentedFps", "worstTileSsim", "vmaf", "ssim", "psnr", "vmafMean",
    "vmafP5", "ssimMean", "ssimP5", "edgeFidelity", "exactFrameRatio"]);
  const higherIsWorse = new Set(["firstFrameMs", "encodeP95Ms", "latencyP95Ms", "actionToPhotonP95Ms",
    "sourceToPresentedP50Ms", "sourceToPresentedP95Ms", "receiveToPresentP50Ms", "receiveToPresentP95Ms",
    "encodedBitrateCv", "wireBitrateCv", "frameGapP95Ms", "frameGapMaxMs", "hostCpuPct",
    "gpuUtilizationMeanPct", "gpuEncoderMeanPct", "privateMemoryPeakMb", "vramPeakMb",
    "gpuPowerMeanWatts", "memorySlopeMbPerMinute", "ocrCer", "colorError", "unchangedRegionError",
    "blockArtifactSeverity", "presentationCadenceP95Ms"]);
  for (const field of fields) {
    const values = repetitions.map(item => item[field]).filter(Number.isFinite);
    aggregate[field] = values.length === 0 ? median(values)
      : lowerIsWorse.has(field) ? Math.min(...values)
      : higherIsWorse.has(field) ? Math.max(...values)
      : median(values);
    aggregate.noise[field] = medianAbsoluteDeviation(values);
  }
  for (const field of ["freezes", "freezeCount", "duplicateFrames", "droppedFrames", "decoderResets", "dependencyLosses", "spatialTransitions"])
    aggregate[field] = Math.max(...repetitions.map(item => Number(item[field]) || 0));
  aggregate.queueSlopePerMinute = Math.max(0, ...repetitions.map(item => item.queueSlopePerMinute).filter(Number.isFinite));
  aggregate.memorySlopeMbPerMinute = Math.max(0, ...repetitions.map(item => item.memorySlopeMbPerMinute).filter(Number.isFinite));
  aggregate.bandwidthVerified = aggregateVerification(
    repetitions.map(item => item.bandwidthVerified));
  aggregate.performanceContaminated = repetitions.some(item => item.performanceContaminated === true);
  aggregate.latencyAccuracy = aggregate.performanceContaminated ? "possibly-inaccurate" : "verified";
  const presentationFields = [
    "decodedFrames", "presentedFrames", "immediateAttempts", "immediatePresented",
    "rafEnqueued", "rafCoalesced", "rafLateDrops",
  ];
  const presentationSamples = repetitions.map(item => item.wssPresentation).filter(Boolean);
  if (presentationSamples.length > 0) {
    const complete = presentationSamples.length === repetitions.length &&
      presentationSamples.every(presentation => presentation?.protocol === 1 &&
        presentationFields.every(field => Number.isFinite(Number(presentation[field]))));
    aggregate.wssPresentation = complete ? {
      protocol: 1,
      ...Object.fromEntries(presentationFields.map(field => [
        field,
        presentationSamples.reduce((total, presentation) => total + Number(presentation[field]), 0),
      ])),
    } : null;
  }
  const actionProbes = repetitions.map(item => item.actionProbe).filter(Boolean);
  if (actionProbes.length) {
    const results = [...new Set(actionProbes.map(probe => probe.result))];
    const reasons = [...new Set(actionProbes.map(probe => probe.reason).filter(Boolean))];
    aggregate.actionProbe = {
      result: results.length === 1 ? results[0] : "mixed",
      ...(reasons.length === 1 ? { reason: reasons[0] } : {}),
    };
  }
  aggregate.activityEvidence = repetitions.map(item => item.activityEvidence).filter(Boolean);
  aggregate.mappedQualitySamples = Math.min(...repetitions.map(item =>
    Number(item.mappedQualitySamples) || 0));
  aggregate.hybridQualitySamples = Math.min(...repetitions.map(item =>
    Number(item.hybridQualitySamples) || 0));
  const hybridActivities = repetitions.map(item => item.hybridActivity).filter(Boolean);
  if (hybridActivities.length) {
    aggregate.hybridActivity = {
      composites: Math.min(...hybridActivities.map(activity => Number(activity.composites) || 0)),
      decodedTiles: Math.min(...hybridActivities.map(activity =>
        Number(activity.decodedTiles) || 0)),
      decoderErrors: Math.max(...hybridActivities.map(activity =>
        Number(activity.decoderErrors) || 0)),
      decoderPressureBypasses: Math.max(...hybridActivities.map(activity =>
        Number(activity.decoderPressureBypasses) || 0)),
      hevcBytes: Math.min(...hybridActivities.map(activity => Number(activity.hevcBytes) || 0)),
      staleDrawRejects: Math.max(...hybridActivities.map(activity =>
        Number(activity.staleDrawRejects) || 0)),
      staleComposites: Math.max(...hybridActivities.map(activity =>
        Number(activity.staleComposites) || 0)),
    };
  }
  const hostHybridActivities = repetitions.map(item => item.hostHybridActivity).filter(Boolean);
  if (hostHybridActivities.length) {
    const aggregateHostField = (field, reduce) => {
      const values = hostHybridActivities.map(activity => Number(activity[field]));
      return values.every(Number.isFinite) ? reduce(values) : undefined;
    };
    const hostMin = field => aggregateHostField(field, values => Math.min(...values));
    const hostMax = field => aggregateHostField(field, values => Math.max(...values));
    aggregate.hostHybridActivity = {
      mode: hostHybridActivities[0].mode,
      frames: Math.min(...hostHybridActivities.map(activity => Number(activity.frames) || 0)),
      analysisP95Us: Math.max(...hostHybridActivities.map(activity =>
        Number(activity.analysisP95Us) || 0)),
      analysisMaxUs: Math.max(...hostHybridActivities.map(activity =>
        Number(activity.analysisMaxUs) || 0)),
      pngUsMax: Math.max(...hostHybridActivities.map(activity =>
        Number(activity.pngUsMax) || 0)),
      motionBypasses: Math.min(...hostHybridActivities.map(activity =>
        Number(activity.motionBypasses) || 0)),
      cropRemaps: Math.min(...hostHybridActivities.map(activity =>
        Number(activity.cropRemaps) || 0)),
      cursorNeutralFrames: Math.min(...hostHybridActivities.map(activity =>
        Number(activity.cursorNeutralFrames) || 0)),
      budgetSkippedUpserts: Math.max(...hostHybridActivities.map(activity =>
        Number(activity.budgetSkippedUpserts) || 0)),
      validationFrames: Math.min(...hostHybridActivities.map(activity =>
        Number(activity.validationFrames) || 0)),
      validationUnavailableFrames: Math.max(...hostHybridActivities.map(activity =>
        Number(activity.validationUnavailableFrames) || 0)),
      validationFailedFrames: Math.max(...hostHybridActivities.map(activity =>
        Number(activity.validationFailedFrames) || 0)),
      validationSubmitP95Us: Math.max(...hostHybridActivities.map(activity =>
        Number(activity.validationSubmitP95Us) || 0)),
      validationSubmitMaxUs: Math.max(...hostHybridActivities.map(activity =>
        Number(activity.validationSubmitMaxUs) || 0)),
      validationCollectP95Us: Math.max(...hostHybridActivities.map(activity =>
        Number(activity.validationCollectP95Us) || 0)),
      validationCollectMaxUs: Math.max(...hostHybridActivities.map(activity =>
        Number(activity.validationCollectMaxUs) || 0)),
      candidates: hostMax("candidates"),
      reissuedTiles: hostMax("reissuedTiles"),
      upserts: hostMax("upserts"),
      invalidations: hostMax("invalidations"),
      pngBytes: hostMax("pngBytes"),
      pngUsTotal: hostMax("pngUsTotal"),
      hevcTileBytes: hostMax("hevcTileBytes"),
      hevcTileCopyUsTotal: hostMax("hevcTileCopyUsTotal"),
      hevcTileCopyUsMax: hostMax("hevcTileCopyUsMax"),
      hevcTileEncodeUsTotal: hostMax("hevcTileEncodeUsTotal"),
      hevcTileEncodeUsMax: hostMax("hevcTileEncodeUsMax"),
      hevcTileEncoderOpens: hostMin("hevcTileEncoderOpens"),
      hevcTileEncoderFailures: hostMax("hevcTileEncoderFailures"),
      sidecarBytes: hostMax("sidecarBytes"),
      suppressions: hostMin("suppressions"),
      reactivations: hostMin("reactivations"),
      permanentInvalidations: hostMax("permanentInvalidations"),
      staleReactivationRejects: hostMax("staleReactivationRejects"),
      pngBytesAvoided: hostMin("pngBytesAvoided"),
      cachedBytesRetained: hostMin("cachedBytesRetained"),
      deferredValidationReadyFrames: hostMin("deferredValidationReadyFrames"),
      deferredValidationPendingFrames: hostMax("deferredValidationPendingFrames"),
      deferredValidationAgeP95Frames: hostMax("deferredValidationAgeP95Frames"),
      deferredValidationAgeMaxFrames: hostMax("deferredValidationAgeMaxFrames"),
      deferredValidationPendingMaxAgeFrames:
        hostMax("deferredValidationPendingMaxAgeFrames"),
      ...(hostHybridActivities.at(-1)?.lastValidationSequence !== undefined
        ? { lastValidationSequence: hostHybridActivities.at(-1).lastValidationSequence } : {}),
    };
  }
  const roiActivities = repetitions.map(item => item.roiActivity).filter(Boolean);
  if (roiActivities.length) {
    const count = field => Math.min(...roiActivities.map(activity => Number(activity[field]) || 0));
    aggregate.roiActivity = {
      mode: roiActivities[0].mode,
      evaluatedFrames: count("evaluatedFrames"),
      mappedFrames: count("mappedFrames"),
      missingDamageFrames: Math.max(...roiActivities.map(activity => Number(activity.missingDamageFrames) || 0)),
      emptyDamageFrames: Math.max(...roiActivities.map(activity => Number(activity.emptyDamageFrames) || 0)),
      idrFrames: Math.max(...roiActivities.map(activity => Number(activity.idrFrames) || 0)),
      coverageRejectedFrames: Math.max(...roiActivities.map(activity => Number(activity.coverageRejectedFrames) || 0)),
      unsupportedFrames: Math.max(...roiActivities.map(activity => Number(activity.unsupportedFrames) || 0)),
      ...(roiActivities.every(activity => activity.source === roiActivities[0].source)
        ? { source: roiActivities[0].source } : { source: "mixed" }),
      temporalFrames: count("temporalFrames"),
      temporalPrimingFrames: Math.max(...roiActivities.map(activity =>
        Number(activity.temporalPrimingFrames) || 0)),
      analysisP95Us: Math.max(...roiActivities.map(activity => Number(activity.analysisP95Us) || 0)),
      analysisMaxUs: Math.max(...roiActivities.map(activity => Number(activity.analysisMaxUs) || 0)),
      ...Object.fromEntries(["lastSequence", "lastReason", "lastChangedCtbs", "lastTotalCtbs",
        "lastMinDelta", "lastMaxDelta", "lastFeedbackSequence", "lastFeedbackAgeFrames",
        "lastFeedbackBudgetPolicy", "lastFeedbackWireOccupancyBps"]
        .filter(field => roiActivities.at(-1)?.[field] !== undefined)
        .map(field => [field, roiActivities.at(-1)[field]])),
    };
    const feedbackCounts = ["feedbackSubmittedFrames", "feedbackReadyFrames", "feedbackLateFrames",
      "feedbackSkippedFrames", "feedbackAdjustedFrames", "feedbackWarmupFrames",
      "feedbackMotionBypasses", "feedbackMissingFrames", "feedbackEligibleCtbs",
      "feedbackRefinedCtbs", "feedbackFundedCtbs", "feedbackSpendHeadroomFrames",
      "feedbackNetZeroFrames"];
    for (const field of feedbackCounts) {
      if (roiActivities.some(activity => activity[field] !== undefined))
        aggregate.roiActivity[field] = count(field);
    }
    for (const field of ["feedbackSubmitP95Us", "feedbackSubmitMaxUs",
      "feedbackReadbackP95Us", "feedbackReadbackMaxUs", "feedbackWireOccupancyP95Bps",
      "feedbackWireOccupancyMaxBps"]) {
      if (roiActivities.some(activity => activity[field] !== undefined))
        aggregate.roiActivity[field] = Math.max(...roiActivities.map(activity =>
          Number(activity[field]) || 0));
    }
    if (roiActivities.some(activity => activity.reconstructionActive !== undefined))
      aggregate.roiActivity.reconstructionActive = roiActivities.every(activity =>
        activity.reconstructionActive === true);
  }
  const encodeStages = repetitions.map(item => item.encodeStages).filter(Boolean);
  if (encodeStages.length) {
    aggregate.encodeStages = {
      frames: Math.min(...encodeStages.map(stages => Number(stages.frames) || 0)),
    };
    for (const name of ["register", "map", "submit", "lock", "copy", "unmap", "unregister"]) {
      const stages = encodeStages.map(value => value[name]).filter(Boolean);
      aggregate.encodeStages[name] = {
        count: Math.max(...stages.map(stage => Number(stage.count) || 0)),
        totalMs: median(stages.map(stage => stage.totalMs).filter(Number.isFinite)),
        meanMs: median(stages.map(stage => stage.meanMs).filter(Number.isFinite)),
        maxMs: Math.max(0, ...stages.map(stage => stage.maxMs).filter(Number.isFinite)),
      };
    }
  }
  aggregate.sourceParityLoss = summarizeSourceParityLoss(aggregate);
  return aggregate;
}

function aggregateRepetitions(repetitions, candidate) {
  return aggregateExperimentRepetitions(repetitions, candidate);
}

export async function runLiveSweep(options) {
  const session = new LiveStreamLabSession(options);
  const base = {
    id: "baseline", preset: "p3", tuning: "ultra-low-latency", multipass: "disabled",
    vbvFrames: 1, gopSeconds: 2, aqStrength: 8, temporalAq: false, bitrateMbps: 6,
  };
  const results = [];
  const completedUnits = options.resumeState?.units ?? {};
  const checkpointContext = { completedUnits, onCheckpoint: options.onCheckpoint };
  const checkpoint = (unitId, operation, reconcile = null) =>
    runCheckpointedUnit(checkpointContext, unitId, reconcile, operation);
  try {
    await session.start("wss");
    const baselineMatrixRuns = [];
    for (const entry of CAMPAIGN_BASELINES) for (let repetition = 1; repetition <= entry.repetitions; repetition++) {
      const id = `baseline-${entry.label}-r${repetition}`;
      const configuration = constrainedConfiguration(base, entry.bandwidthMbps, "wss");
      const run = await checkpoint(`baseline:${entry.label}:r${repetition}`, () =>
        runSweepCandidate(session, { id, configuration }, options, entry.bandwidthMbps));
      baselineMatrixRuns.push({ ...run, bandwidthMbps: entry.bandwidthMbps,
        requestedBitrateMbps: base.bitrateMbps, effectiveBitrateMbps: configuration.bitrateMbps });
    }
    const baseline = aggregateRepetitions(baselineMatrixRuns.filter(run => run.bandwidthMbps === null), base);
    const baselineMatrix = [null, 2, 4, 8, 12].map(bandwidthMbps => ({ bandwidthMbps,
      ...aggregateRepetitions(baselineMatrixRuns.filter(run => run.bandwidthMbps === bandwidthMbps),
        constrainedConfiguration(base, bandwidthMbps, "wss")) }));
    const initial = buildEncoderRound(base, "presetTuning");
    for (const candidate of initial) results.push(await checkpoint(`screen:presetTuning:${candidate.id}`, () => runSweepCandidate(session, candidate, options, null)));
    attachComparisons(baseline, results);
    let survivors = rankCandidates([baseline, ...results.filter(result => result.comparison.accepted)]).slice(0, 3);
    if (!survivors.length) throw new StreamLabUnavailable("no-valid-candidates", "Every first-round encoder candidate failed a hard gate.", results);
    let championConfig = survivors[0].configuration;
    for (const dimension of ["multipass", "vbvFrames", "gopSeconds", "aqStrength", "temporalAq"]) {
      const round = [];
      for (const candidate of buildEncoderRound(championConfig, dimension)) round.push(await checkpoint(`screen:${dimension}:${candidate.id}`, () => runSweepCandidate(session, candidate, options, null)));
      results.push(...round);
      attachComparisons(baseline, round);
      const valid = rankCandidates(round.filter(candidate => candidate.comparison.accepted)).slice(0, 1);
      if (valid.length) championConfig = valid[0].configuration;
    }
    const bitrateRound = [];
    for (const candidate of buildEncoderRound(championConfig, "bitrateMbps")) bitrateRound.push(await checkpoint(`screen:bitrateMbps:${candidate.id}`, () => runSweepCandidate(session, candidate, options, null)));
    results.push(...bitrateRound);
    attachComparisons(baseline, bitrateRound);
    survivors = rankCandidates([baseline, ...results.filter(result => result.comparison?.accepted)]).slice(0, 3);
    if (!survivors.length) throw new StreamLabUnavailable("no-champion", "No encoder candidate survived the search.");
    const finalistRuns = [];
    for (const finalist of survivors) for (const bandwidthMbps of [2, 4, 8, 12]) {
      finalistRuns.push(await checkpoint(`finalist:${finalist.id}:${bandwidthMbps}`, async () => {
        const effective = constrainedConfiguration(finalist.configuration, bandwidthMbps, "wss");
        const stabilized = await stabilizeEncoderForCap(session, effective, bandwidthMbps, options);
        const performance = await session.collectPerformance(options.finalistSeconds ?? 15);
        const perf = summarizePerformance(performance.samples, {
          transport: "wss", firstFrameMs: session.lastFirstFrameMs,
          actionToPhotonSamples: performance.actionToPhotonSamples, bandwidthMbps,
          encoderBitrateMbps: stabilized.stabilizedEncoderBitrateMbps, qosPolicy: session.qosPolicy,
        });
        assertShapingValid(perf, bandwidthMbps, session.qosPolicy, stabilized);
        const quality = await session.collectQuality(`${finalist.id}-${bandwidthMbps}m`);
        return { id: finalist.id, configuration: finalist.configuration, bandwidthMbps,
          ...stabilized, effectiveBitrateMbps: stabilized.stabilizedEncoderBitrateMbps,
          ...combinePerformanceAndQuality(perf, quality) };
      }));
    }
    const finalistAggregates = survivors.map(finalist => aggregateFinalist(
      finalist.id, finalist.configuration, finalistRuns.filter(run => run.id === finalist.id)));
    const champion = rankCandidates(finalistAggregates)[0];
    const soakMetrics = await checkpoint(`soak:${champion.id}:4`, async () => {
      const effective = constrainedConfiguration(champion.configuration, 4, "wss");
      const stabilized = await stabilizeEncoderForCap(session, effective, 4, options);
      const soak = await session.collectPerformance(options.soakSeconds ?? 600);
      const metrics = summarizePerformance(soak.samples, { transport: "wss", firstFrameMs: session.lastFirstFrameMs,
        actionToPhotonSamples: soak.actionToPhotonSamples, bandwidthMbps: 4,
        encoderBitrateMbps: stabilized.stabilizedEncoderBitrateMbps, qosPolicy: session.qosPolicy });
      assertShapingValid(metrics, 4, session.qosPolicy, stabilized);
      return { ...stabilized, ...metrics };
    });
    const stability = [];
    const stabilityCheckpoint = async (unitId, operation, reconcile = null) => ({ unitId,
      ...await checkpoint(unitId, operation, reconcile) });
    const prepareWorkChampion = async () => {
      await session.switchTransport("wss", { adaptive: false });
      await session.apply(constrainedConfiguration(champion.configuration, 4, "wss"), 4, "wss");
    };
    stability.push(await stabilityCheckpoint("stability:decoder-fault-recovery",
      () => session.collectDecoderFaultRecovery(), prepareWorkChampion));
    stability.push(await stabilityCheckpoint("stability:sharp-viewport-2x",
      () => session.collectSharpViewportComparison(champion.id), prepareWorkChampion));

    const adaptiveDwells = [];
    for (const [index, bandwidthMbps] of [8, 4, 2, 4, 8].entries()) {
      const occurrence = [8, 4, 2, 4, 8].slice(0, index + 1).filter(value => value === bandwidthMbps).length;
      const suffix = bandwidthMbps === 2 ? "2m" : `${bandwidthMbps}m-${occurrence}`;
      const unitId = `stability:adaptive-bandwidth:${suffix}`;
      adaptiveDwells.push(await stabilityCheckpoint(unitId, () =>
        session.applyAdaptiveShape(bandwidthMbps, options.adaptiveDwellSeconds ?? 120),
        () => session.switchTransport("wss", { adaptive: true })));
    }
    stability.push(await stabilityCheckpoint("stability:adaptive-bandwidth-staircase", async () => ({
      result: adaptiveDwells.some(unit => unit.result === "unsupported") ? "unsupported" : "complete",
      sequenceMbps: [8, 4, 2, 4, 8], dwells: adaptiveDwells,
    }), () => session.switchTransport("wss", { adaptive: true })));

    const gameBase = { ...champion.configuration, codec: "hevc", streamMode: "game", fps: 60, gopSeconds: 1, bitrateMbps: 8 };
    const wssGameEffective = constrainedConfiguration(gameBase, 8, "wss");
    stability.push(await stabilityCheckpoint("stability:wss-gaming", async () => {
      const performance = await session.collectPerformance(options.regressionSeconds ?? 15);
      return { result: "complete", requestedBitrateMbps: 8, effectiveBitrateMbps: wssGameEffective.bitrateMbps,
        ...summarizePerformance(performance.samples, { transport: "wss", firstFrameMs: session.lastFirstFrameMs,
          actionToPhotonSamples: performance.actionToPhotonSamples, bandwidthMbps: 8, encoderBitrateMbps: wssGameEffective.bitrateMbps }) };
    }, async () => {
      await session.switchTransport("wss", { adaptive: false });
      await session.apply(wssGameEffective, 8, "wss");
    }));

    stability.push(await stabilityCheckpoint("stability:webrtc-gaming-unshaped", async () => {
      const performance = await session.collectPerformance(options.regressionSeconds ?? 15);
      return { result: "complete", bandwidthMbps: null, ...summarizePerformance(performance.samples, {
        transport: "webrtc", firstFrameMs: session.lastFirstFrameMs, actionToPhotonSamples: performance.actionToPhotonSamples,
      }) };
    }, async () => {
      await session.switchTransport("webrtc", { relay: false, adaptive: false });
      await session.apply(gameBase, null, "webrtc", { relay: null });
    }));

    stability.push(await stabilityCheckpoint("stability:webrtc-h264-diagnostic",
      () => session.collectWebRtcDiagnosticSmoke(options.compatibilitySeconds ?? 5)));

    const rtcRelayEffective = constrainedConfiguration(gameBase, 8, "webrtc");
    stability.push(await stabilityCheckpoint("stability:webrtc-gaming-relay-8m", async () => {
      const performance = await session.collectPerformance(options.regressionSeconds ?? 15);
      return { result: "complete", bandwidthMbps: 8, requestedBitrateMbps: 8, effectiveBitrateMbps: rtcRelayEffective.bitrateMbps,
        ...summarizePerformance(performance.samples, { transport: "webrtc", firstFrameMs: session.lastFirstFrameMs,
          actionToPhotonSamples: performance.actionToPhotonSamples, bandwidthMbps: 8, encoderBitrateMbps: rtcRelayEffective.bitrateMbps }) };
    }, async () => {
      await session.switchTransport("webrtc", { relay: true, adaptive: false });
      await session.apply(rtcRelayEffective, 8, "webrtc", { relay: true });
    }));

    const h264 = await stabilityCheckpoint("stability:h264-fallback-smoke", async () => {
      return session.collectH264FallbackSmoke(options.compatibilitySeconds ?? 5);
    }, () => session.switchTransport("wss", { adaptive: false }));
    stability.push(h264);

    const idleUnitId = "stability:post-cleanup-idle";
    const idle = { unitId: idleUnitId, ...await runFreshCheckpoint(checkpointContext, idleUnitId,
      () => session.stop(),
      () => collectIdleEvidence(options.idleSeconds ?? 60, {
        cleanupEvidence: session.cleanupEvidence,
        preCampaignSystem: session.preCampaignSystem,
      })) };
    stability.push(idle);
    return {
      result: "complete", strategy: "adaptive-successive-halving", champion, finalists: finalistAggregates,
      baseline, baselineMatrix, results, finalistRuns, soak: soakMetrics, stability,
      regressions: stability.filter(unit => ["wss", "webrtc"].includes(unit.transport)),
      compatibilitySmoke: h264,
    };
  } finally { await session.stop(); }
}

function attachComparisons(baseline, candidates) {
  for (const candidate of candidates) candidate.comparison = compareCandidates(baseline, candidate, { noise: baseline.noise });
}

function aggregateFinalist(id, configuration, runs) {
  const value = field => median(runs.map(run => run[field]).filter(Number.isFinite));
  return {
    id, configuration, runs,
    transport: "wss", codec: "hevc",
    firstFrameMs: Math.max(...runs.map(run => run.firstFrameMs)),
    presentedFps: value("presentedFps"), encodeP95Ms: Math.max(...runs.map(run => run.encodeP95Ms)),
    latencyP95Ms: Math.max(...runs.map(run => run.latencyP95Ms)),
    actionToPhotonP95Ms: Math.max(...runs.map(run => run.actionToPhotonP95Ms)),
    ocrCer: value("ocrCer"), worstTileSsim: Math.min(...runs.map(run => run.worstTileSsim)),
    vmaf: value("vmaf"), vmafMean: value("vmafMean"), vmafP5: Math.min(...runs.map(run => run.vmafP5 ?? Infinity)),
    ssimMean: value("ssimMean"), ssimP5: Math.min(...runs.map(run => run.ssimP5 ?? Infinity)),
    edgeFidelity: value("edgeFidelity"), colorError: value("colorError"),
    unchangedRegionError: value("unchangedRegionError"), blockArtifactSeverity: value("blockArtifactSeverity"),
    exactFrameRatio: Math.min(...runs.map(run => run.exactFrameRatio ?? 0)), wireBitrateMbps: value("wireBitrateMbps"),
    encodedBitrateCv: value("encodedBitrateCv"), wireBitrateCv: value("wireBitrateCv"),
    frameGapP95Ms: Math.max(...runs.map(run => run.frameGapP95Ms ?? 0)),
    frameGapMaxMs: Math.max(...runs.map(run => run.frameGapMaxMs ?? 0)),
    hostCpuPct: value("hostCpuPct"), gpuUtilizationMeanPct: value("gpuUtilizationMeanPct"),
    gpuEncoderMeanPct: value("gpuEncoderMeanPct"), privateMemoryPeakMb: Math.max(...runs.map(run => run.privateMemoryPeakMb ?? 0)),
    vramPeakMb: Math.max(...runs.map(run => run.vramPeakMb ?? 0)), gpuPowerMeanWatts: value("gpuPowerMeanWatts"),
    freezes: Math.max(...runs.map(run => run.freezes)), decoderResets: Math.max(...runs.map(run => run.decoderResets)),
    dependencyLosses: Math.max(...runs.map(run => run.dependencyLosses)), spatialTransitions: Math.max(...runs.map(run => run.spatialTransitions)),
    queueSlopePerMinute: Math.max(...runs.map(run => run.queueSlopePerMinute)),
    memorySlopeMbPerMinute: Math.max(...runs.map(run => run.memorySlopeMbPerMinute)),
    bandwidthVerified: aggregateVerification(runs.map(run => run.bandwidthVerified)),
  };
}

async function runSweepCandidate(session, candidate, options, bandwidthMbps = null) {
  const stabilized = await stabilizeEncoderForCap(session, candidate.configuration ?? candidate, bandwidthMbps, options);
  const performance = await session.collectPerformance(options.screeningSeconds ?? 15);
  const perf = summarizePerformance(performance.samples, { transport: "wss", firstFrameMs: session.lastFirstFrameMs,
    actionToPhotonSamples: performance.actionToPhotonSamples, bandwidthMbps,
    encoderBitrateMbps: stabilized.stabilizedEncoderBitrateMbps, qosPolicy: session.qosPolicy });
  assertShapingValid(perf, bandwidthMbps, session.qosPolicy, stabilized);
  const quality = await session.collectQuality(candidate.id, options.screeningScenes ?? ["ui-text", "scrolling", "localized-change"]);
  return { id: candidate.id, configuration: candidate.configuration ?? candidate,
    ...stabilized, ...combinePerformanceAndQuality(perf, quality) };
}

function constrainedConfiguration(configuration, bandwidthMbps, transport) {
  const requestedBitrateMbps = configuration.requestedBitrateMbps ?? configuration.bitrateMbps ?? 6;
  const bitrateMbps = transport === "wss" && Number.isFinite(bandwidthMbps)
    ? initialEncoderTargetMbps(requestedBitrateMbps, bandwidthMbps)
    : effectiveEncoderBitrateMbps(requestedBitrateMbps, bandwidthMbps, transport);
  return { ...configuration, requestedBitrateMbps, bitrateMbps };
}

export async function collectIdleEvidence(seconds, context = {}) {
  const samples = [];
  const started = performance.now();
  do {
    samples.push({ elapsedMs: performance.now() - started, system: sampleSystemTelemetry() });
    if (performance.now() - started >= seconds * 1_000) break;
    await sleep(Math.min(1_000, Math.max(0, seconds * 1_000 - (performance.now() - started))));
  } while (true);
  const values = field => samples.map(sample => Number(sample.system?.[field])).filter(Number.isFinite);
  const gpu = values("gpuUtilizationPct"), encoder = values("encoderUtilizationPct");
  const memory = values("privateMemoryBytes"), vram = values("vramUsedMb"), power = values("powerWatts");
  const cleanupEvidence = context.cleanupEvidence ?? null;
    const evidenceAvailable = ["displays", "qos", "chrome"].every(
    key => cleanupEvidence?.evidenceAvailability?.[key] === true);
  const resourcesReleased = cleanupResourcesReleased(cleanupEvidence);
  const baselineMemoryBytes = Number(context.preCampaignSystem?.privateMemoryBytes);
  const currentMemoryBytes = memory.at(-1);
  const hostMemoryDeltaMb = Number.isFinite(baselineMemoryBytes) && Number.isFinite(currentMemoryBytes)
    ? (currentMemoryBytes - baselineMemoryBytes) / 1024 / 1024 : null;
  const telemetryAvailable = encoder.length > 0 && memory.length > 0 && Number.isFinite(baselineMemoryBytes);
  const thresholdsPassed = mean(encoder) <= 1 && hostMemoryDeltaMb <= 50;
  const result = !evidenceAvailable || !telemetryAvailable ? "unsupported" :
    !resourcesReleased || !thresholdsPassed ? "fail" : "complete";
  return { result, seconds, sampleCount: samples.length,
    telemetryAvailability: { process: memory.length > 0, gpu: gpu.length > 0, encoder: encoder.length > 0 },
    gpuUtilizationMeanPct: mean(gpu), gpuEncoderMeanPct: mean(encoder),
    privateMemoryPeakMb: memory.length ? Math.max(...memory) / 1024 / 1024 : null,
    privateMemoryLastMb: Number.isFinite(currentMemoryBytes) ? currentMemoryBytes / 1024 / 1024 : null,
    hostMemoryBaselineMb: Number.isFinite(baselineMemoryBytes) ? baselineMemoryBytes / 1024 / 1024 : null,
    hostMemoryDeltaMb,
    cleanupEvidence,
    vramPeakMb: vram.length ? Math.max(...vram) : null, gpuPowerMeanWatts: mean(power), samples };
}
