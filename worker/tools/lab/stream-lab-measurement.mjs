// Pure alignment, cohort-readiness, and scoring policies; no process ownership.
import { SCENES, median, percentile } from "./stream-lab-core.mjs";
import { selectParetoFrontier } from "./stream-lab-parity.mjs";
import { StreamLabUnavailable } from "./stream-lab-lifecycle.mjs";
export const HYBRID_SCENES = Object.freeze(["source-code", "chrome-text", "chrome-canvas", "generated-video"]);

export function chooseOptimizationFinalists(candidates, limit, noise = {}) {
  if (!Array.isArray(candidates)) throw new TypeError("optimization candidates must be an array");
  if (!Number.isInteger(limit) || limit < 1) throw new RangeError("finalist limit must be positive");
  const eligible = candidates.filter(candidate => candidate?.validationAccepted === true);
  const frontier = selectParetoFrontier(eligible, { noise });
  return frontier.sort((left, right) =>
    finiteDescending(left.vmafP5, right.vmafP5) ||
    finiteDescending(left.worstTileSsim, right.worstTileSsim) ||
    finiteAscending(left.latencyP95Ms, right.latencyP95Ms) ||
    String(left.id).localeCompare(String(right.id))).slice(0, limit);
}

function finiteDescending(left, right) {
  return (Number.isFinite(right) ? right : -Infinity) - (Number.isFinite(left) ? left : -Infinity);
}

function finiteAscending(left, right) {
  return (Number.isFinite(left) ? left : Infinity) - (Number.isFinite(right) ? right : Infinity);
}

export function summarizeAlignmentFailure(error) {
  if (!error) return null;
  return {
    code: typeof error.code === "string" ? error.code : null,
    message: String(error.message || error),
    detail: error.detail ?? null,
  };
}

function u32SequenceAfter(current, previous) {
  const delta = ((current >>> 0) - (previous >>> 0)) >>> 0;
  return delta > 0 && delta < 0x8000_0000;
}

function wssCohortResyncs(snapshot) {
  return Number(snapshot?.resyncs ?? snapshot?.worker?.resyncs);
}

function wssCohortContinuityReasons(admitted, current, options = {}) {
  const reasons = [];
  const admittedMedia = admitted?.wssMedia;
  const currentMedia = current?.wssMedia;
  if (current?.route !== "wss") reasons.push("route-not-wss");
  if (current?.signaling?.registered !== true) reasons.push("signaling-unregistered");
  if (Number(currentMedia?.protocol) !== 4) reasons.push("protocol-not-v4");
  if (currentMedia?.v4?.socketOpen !== true) reasons.push("media-socket-closed");
  if (currentMedia?.recoveryState !== "healthy") reasons.push("recovery-state-unhealthy");
  if (Number(current?.signaling?.registeredAtMs) !== Number(admitted?.signaling?.registeredAtMs))
    reasons.push("signaling-epoch-changed");
  if (options.allowMediaGenerationChange !== true &&
      Number(current?.mediaGeneration) !== Number(admitted?.mediaGeneration))
    reasons.push("media-generation-changed");
  if (options.allowResyncChange !== true &&
      wssCohortResyncs(current) !== wssCohortResyncs(admitted))
    reasons.push("decoder-resync");
  if (Number(currentMedia?.v4?.gateDrops) !== Number(admittedMedia?.v4?.gateDrops))
    reasons.push("dependency-loss");
  if (options.requireFreshPresentation === true) {
    const renderedAgeMs = Number(current?.renderedAgeMs);
    const maxRenderedAgeMs = Number(options.maxRenderedAgeMs ?? 500);
    if (!Number.isFinite(renderedAgeMs) || renderedAgeMs > maxRenderedAgeMs)
      reasons.push("rendered-frame-stale");
  }
  return [...new Set(reasons)];
}

export function evaluateWssCohortContinuity(admitted, current, options = {}) {
  const reasons = wssCohortContinuityReasons(admitted, current, options);
  return {
    accepted: reasons.length === 0,
    reasons,
    mediaSocketOpen: current?.wssMedia?.v4?.socketOpen === true,
    resyncs: wssCohortResyncs(current),
    renderedAgeMs: Number(current?.renderedAgeMs),
    mediaGeneration: Number(current?.mediaGeneration),
    registeredAtMs: Number(current?.signaling?.registeredAtMs),
  };
}

export function evaluateWssCohortProgress(previous, current, options = {}) {
  const reasons = wssCohortContinuityReasons(previous, current, {
    ...options,
    requireFreshPresentation: true,
  });
  const previousMedia = previous?.wssMedia;
  const currentMedia = current?.wssMedia;
  if (!u32SequenceAfter(Number(currentMedia?.reconstructed), Number(previousMedia?.reconstructed)))
    reasons.push("reconstruction-stalled");
  if (!u32SequenceAfter(Number(currentMedia?.presented), Number(previousMedia?.presented)))
    reasons.push("presentation-stalled");
  const uniqueReasons = [...new Set(reasons)];
  return {
    accepted: uniqueReasons.length === 0,
    reasons: uniqueReasons,
    mediaSocketOpen: currentMedia?.v4?.socketOpen === true,
    resyncs: wssCohortResyncs(current),
    renderedAgeMs: Number(current?.renderedAgeMs),
  };
}

export function advanceWssCohortAdmission(state, current, options = {}) {
  const requiredProgressSamples = Number(options.requiredProgressSamples ?? 3);
  if (!state?.lastSnapshot) {
    return {
      accepted: false,
      consecutiveProgressSamples: 0,
      requiredProgressSamples,
      observation: {
        accepted: false,
        reasons: ["baseline-required"],
        mediaSocketOpen: current?.wssMedia?.v4?.socketOpen === true,
      },
      lastSnapshot: current,
    };
  }
  const observation = evaluateWssCohortProgress(state.lastSnapshot, current, options);
  const consecutiveProgressSamples = observation.accepted
    ? Number(state.consecutiveProgressSamples ?? 0) + 1
    : 0;
  return {
    accepted: observation.accepted && consecutiveProgressSamples >= requiredProgressSamples,
    consecutiveProgressSamples,
    requiredProgressSamples,
    observation,
    lastSnapshot: current,
  };
}

function boundedStageDuration(value) {
  return Number.isFinite(value) && value >= 0 && value <= 10_000 ? value : null;
}

function frameStageTrace(diagnostics, phase, capture = {}) {
  const workerTrace = diagnostics?.worker?.frameTrace ?? diagnostics?.frameTrace ?? {};
  const metrics = diagnostics?.benchStatus?.metrics ?? diagnostics?.benchStatus ?? {};
  const capturedAtEpochMs = Number.isSafeInteger(capture.capturedAtEpochMs)
    ? capture.capturedAtEpochMs
    : Number.isFinite(diagnostics?.clock?.epochMs) && Number.isFinite(diagnostics?.glassToGlass?.ageMs)
      ? diagnostics.clock.epochMs - diagnostics.glassToGlass.ageMs
      : null;
  const performanceOriginEpochMs =
    Number.isFinite(diagnostics?.clock?.epochMs) && Number.isFinite(diagnostics?.clock?.performanceMs)
      ? diagnostics.clock.epochMs - diagnostics.clock.performanceMs
      : null;
  const sourceToPresentedMs = boundedStageDuration(
    performanceOriginEpochMs +
      Number(workerTrace.presentedAtMs ?? diagnostics?.renderedTimestamp ?? diagnostics?.nativePresentation?.lastFrameAtMs) -
      capturedAtEpochMs,
  );
  return {
    phase,
    capture: {
      generation: Number.isSafeInteger(capture.generation) ? capture.generation : null,
      sourceFrameId: Number.isSafeInteger(capture.sourceFrameId) ? capture.sourceFrameId : null,
      capturedAtEpochMs: Number.isFinite(capturedAtEpochMs) ? capturedAtEpochMs : null,
    },
    encode: {
      encodedBytes: finiteOrNull(workerTrace.encodedBytes ?? metrics.encodedBytes),
      encodeDurationUs: finiteOrNull(workerTrace.encodeDurationUs ?? metrics.encodeUsMax),
      encodedAtMs: finiteOrNull(workerTrace.encodedAtMs),
      key: typeof workerTrace.key === "boolean" ? workerTrace.key : null,
      roiReason: typeof workerTrace.roiReason === "string" ? workerTrace.roiReason : null,
      roiChangedCtbs: finiteOrNull(workerTrace.roiChangedCtbs),
      roiTotalCtbs: finiteOrNull(workerTrace.roiTotalCtbs),
    },
    network: {
      sequence: finiteOrNull(workerTrace.sequence),
      receivedAtMs: finiteOrNull(workerTrace.receivedAtMs),
    },
    decode: {
      queuedAtMs: finiteOrNull(workerTrace.decodeQueuedAtMs),
      decodedAtMs: finiteOrNull(workerTrace.decodedAtMs),
      queueSize: finiteOrNull(diagnostics?.worker?.decodeQueueSize),
    },
    presentation: {
      presentedAtMs: finiteOrNull(workerTrace.presentedAtMs ?? diagnostics?.renderedTimestamp ?? diagnostics?.nativePresentation?.lastFrameAtMs),
      sourceToPresentedMs,
      ageMs: finiteOrNull(diagnostics?.glassToGlass?.ageMs),
      frameCount: finiteOrNull(diagnostics?.framesDrawn ?? diagnostics?.nativePresentation?.framesPresented),
      hybridCropRevision: finiteOrNull(workerTrace.hybridCropRevision),
      hybridTilesDrawn: finiteOrNull(workerTrace.hybridTilesDrawn),
    },
  };
}

const QUIESCENT_HYBRID_GUARD_SCENES =
  new Set(["motion-60", "chrome-canvas", "generated-video"]);

export function hybridSceneRequirement(mode, scene, cacheMode = "suppress-reactivate") {
  if (mode === "png-static-roi") return "tile";
  if (["png-quiescent-detail", "hevc-quiescent-tiles"].includes(mode)) {
    if (QUIESCENT_HYBRID_GUARD_SCENES.has(scene)) return "guard";
    return cacheMode === "dda-gated" ? "safe" : "tile";
  }
  return "off";
}

export function shouldRunQuiescentInteractionTrials(hybridMode, actionProbe) {
  return hybridMode === "png-quiescent-detail" && actionProbe !== false;
}

export function qualityAnchorAlignmentOptions(mode, scene, acknowledgedSequence) {
  if (!Number.isSafeInteger(acknowledgedSequence) ||
      acknowledgedSequence < 0 || acknowledgedSequence > 0xffff_ffff)
    throw new RangeError("acknowledged quality sequence must be an unsigned 32-bit integer");
  // The acknowledged frame is the clean IDR anchor. Quiescent-detail tile
  // promotion can immediately schedule follow-up snapshots of the same paused
  // source frame, so prove that the anchor was presented from bounded receiver
  // history instead of requiring it to remain the latest presented sequence.
  if (["png-quiescent-detail", "hevc-quiescent-tiles"].includes(mode))
    return { presentedNetworkSequence: acknowledgedSequence >>> 0 };
  void scene;
  return { exactNetworkSequence: acknowledgedSequence >>> 0 };
}

export function quiescentInteractionReady(snapshot, sourceMetadata, baselineHybrid) {
  const hybrid = snapshot?.wssMedia?.hybrid;
  const sequence = Number(sourceMetadata?.frameId);
  const timestamp = Number(sourceMetadata?.epochMs);
  if (!hybrid || !Number.isSafeInteger(sequence) || !Number.isSafeInteger(timestamp))
    return false;
  return snapshot?.glassToGlass?.sequence === (sequence & 0xffff) &&
    snapshot?.glassToGlass?.timestamp === (timestamp >>> 0) &&
    Number(hybrid.activeTiles) > 0 &&
    Number(hybrid.readyTiles) > 0 &&
    Number(hybrid.pendingTiles) === 0 &&
    Number(hybrid.receivedUpserts) > Number(baselineHybrid?.receivedUpserts ?? 0) &&
    Number(hybrid.composites) > Number(baselineHybrid?.composites ?? 0) &&
    Number(hybrid.staleComposites) === 0;
}

export function quiescentTransitionCommitted(before, snapshot) {
  const viewport = snapshot?.viewport;
  const hybrid = snapshot?.wssMedia?.hybrid;
  const beforeRevision = Number(before?.viewport?.revision);
  const revision = Number(viewport?.revision);
  const generation = Number(snapshot?.mediaGeneration);
  if (![beforeRevision, revision, generation].every(Number.isSafeInteger) || !hybrid)
    return false;
  return revision > beforeRevision &&
    viewport?.pending === null &&
    Number(viewport.generation) === generation &&
    Number(hybrid.generation) === generation &&
    Number(hybrid.cropRevision) === revision &&
    Number(hybrid.activeTiles) > 0 &&
    Number(hybrid.readyTiles) > 0 &&
    Number(hybrid.pendingTiles) === 0 &&
    Number(hybrid.composites) > Number(before?.wssMedia?.hybrid?.composites);
}

const QUIESCENT_CURSOR_COUNTERS = ["receivedInvalidations", "receivedUpserts", "pngBytes"];

export function quiescentCursorSweepSettled(before, snapshot, expectedPackets = 4) {
  const beforeStatus = before?.benchStatus;
  const currentStatus = snapshot?.benchStatus;
  const sentBefore = Number(before?.motionSent);
  const sentNow = Number(snapshot?.motionSent);
  return typeof beforeStatus?.runId === "string" &&
    currentStatus?.runId === beforeStatus.runId &&
    Number.isSafeInteger(beforeStatus.sequence) &&
    Number.isSafeInteger(currentStatus.sequence) &&
    currentStatus.sequence > beforeStatus.sequence &&
    Number.isSafeInteger(expectedPackets) &&
    expectedPackets > 0 &&
    Number.isFinite(sentBefore) &&
    Number.isFinite(sentNow) &&
    sentNow - sentBefore >= expectedPackets;
}

export function quiescentCursorCounterChanges(before, snapshot) {
  const first = before?.wssMedia?.hybrid;
  const last = snapshot?.wssMedia?.hybrid;
  return QUIESCENT_CURSOR_COUNTERS.filter(field =>
    Number(last?.[field]) !== Number(first?.[field]));
}

export function exactFrameAlignment(source, diagnostics, phase = "quality", options = {}) {
  const sourceFrameId = Number(source?.frameId);
  const sourceTimestamp = Number(source?.epochMs);
  const sequence = Number(diagnostics?.glassToGlass?.sequence);
  const timestamp = Number(diagnostics?.glassToGlass?.timestamp);
  if (![sourceFrameId, sourceTimestamp, sequence, timestamp].every(Number.isSafeInteger)) {
    throw new StreamLabUnavailable("frame-identity-unavailable",
      "Exact source and decoded frame identity is required for parity capture.", { phase, source, glassToGlass: diagnostics?.glassToGlass ?? null });
  }
  const expectedSequence = sourceFrameId & 0xffff;
  const expectedTimestamp = sourceTimestamp >>> 0;
  if (sequence !== expectedSequence || timestamp !== expectedTimestamp) {
    throw new StreamLabUnavailable("frame-identity-mismatch",
      "The decoded frame does not exactly match the source frame selected for parity capture.",
      { phase, expected: { sequence: expectedSequence, timestamp: expectedTimestamp }, actual: { sequence, timestamp } });
  }
  let workerTrace = diagnostics?.worker?.frameTrace ?? diagnostics?.frameTrace ?? {};
  if (options.presentedNetworkSequence !== undefined) {
    const presentedNetworkSequence = Number(options.presentedNetworkSequence);
    if (!Number.isSafeInteger(presentedNetworkSequence)) {
      throw new StreamLabUnavailable("quality-frame-sequence-unavailable",
        "A presented WSS frame sequence is required for normalized quality capture.",
        { phase, presentedNetworkSequence });
    }
    const traces = [
      workerTrace,
      ...(Array.isArray(diagnostics?.frameTraceHistory) ? diagnostics.frameTraceHistory : []),
    ];
    const presentedTrace = traces.find(trace =>
      Number.isSafeInteger(Number(trace?.sequence)) &&
      (Number(trace.sequence) >>> 0) === (presentedNetworkSequence >>> 0));
    if (!presentedTrace) {
      throw new StreamLabUnavailable("quality-frame-not-presented",
        "The acknowledged normalized quality keyframe was not observed at presentation.",
        { phase, presentedNetworkSequence });
    }
    if (presentedTrace.key !== true) {
      throw new StreamLabUnavailable("quality-frame-not-key",
        "The acknowledged normalized quality anchor was presented without IDR evidence.",
        { phase, presentedNetworkSequence, key: presentedTrace.key });
    }
    workerTrace = presentedTrace;
  }
  if (options.requireDeltaFrame === true && workerTrace.key !== false &&
      options.allowHistoricalDelta !== false) {
    const historicalTraces = Array.isArray(diagnostics?.frameTraceHistory)
      ? diagnostics.frameTraceHistory
      : [];
    const historicalDelta = [...historicalTraces].reverse().find(trace => {
      const traceSequence = Number(trace?.sequence);
      const identitySequence = Number(trace?.presentedIdentity?.sequence);
      const identityTimestamp = Number(trace?.presentedIdentity?.timestamp);
      if (trace?.key !== false || ![traceSequence, identitySequence, identityTimestamp]
        .every(Number.isSafeInteger)) return false;
      if (identitySequence !== expectedSequence || identityTimestamp !== expectedTimestamp)
        return false;
      if (options.afterNetworkSequence !== undefined) {
        const afterNetworkSequence = Number(options.afterNetworkSequence);
        if (!Number.isSafeInteger(afterNetworkSequence) ||
            !u32SequenceAfter(traceSequence, afterNetworkSequence)) return false;
      }
      if (options.exactNetworkSequence !== undefined) {
        const exactNetworkSequence = Number(options.exactNetworkSequence);
        if (!Number.isSafeInteger(exactNetworkSequence) ||
            (traceSequence >>> 0) !== (exactNetworkSequence >>> 0)) return false;
      }
      return true;
    });
    if (historicalDelta) workerTrace = historicalDelta;
  }
  if (options.afterNetworkSequence !== undefined) {
    const networkSequence = Number(workerTrace.sequence);
    const afterNetworkSequence = Number(options.afterNetworkSequence);
    if (![networkSequence, afterNetworkSequence].every(Number.isSafeInteger)) {
      throw new StreamLabUnavailable("quality-frame-sequence-unavailable",
        "A post-request WSS frame sequence is required for normalized quality capture.",
        { phase, networkSequence, afterNetworkSequence });
    }
    if (!u32SequenceAfter(networkSequence, afterNetworkSequence)) {
      throw new StreamLabUnavailable("quality-frame-before-request",
        "The exact decoded frame was presented before the normalized quality keyframe request.",
        { phase, networkSequence, afterNetworkSequence });
    }
  }
  if (options.exactNetworkSequence !== undefined) {
    const networkSequence = Number(workerTrace.sequence);
    const exactNetworkSequence = Number(options.exactNetworkSequence);
    if (![networkSequence, exactNetworkSequence].every(Number.isSafeInteger)) {
      throw new StreamLabUnavailable("quality-frame-sequence-unavailable",
        "An exact WSS frame sequence is required for normalized quality capture.",
        { phase, networkSequence, exactNetworkSequence });
    }
    if ((networkSequence >>> 0) !== (exactNetworkSequence >>> 0)) {
      throw new StreamLabUnavailable("quality-frame-sequence-mismatch",
        "The decoded frame is not the intended normalized quality frame.",
        { phase, networkSequence, exactNetworkSequence });
    }
  }
  if (options.requireDeltaFrame === true && workerTrace.key !== false) {
    throw new StreamLabUnavailable("quality-frame-not-delta",
      "Delta quality capture requires an exact non-IDR frame.",
      { phase, sequence: workerTrace.sequence, key: workerTrace.key });
  }
  let roiActivity = null;
  if (options.requireRoiApplied === true) {
    const changedCtbs = Number(workerTrace.roiChangedCtbs);
    const totalCtbs = Number(workerTrace.roiTotalCtbs);
    if (workerTrace.roiReason !== "applied" ||
        !Number.isSafeInteger(changedCtbs) || !Number.isSafeInteger(totalCtbs) ||
        changedCtbs <= 0 || totalCtbs <= 0) {
      throw new StreamLabUnavailable("quality-roi-map-missing",
        "The exact quality frame did not receive the intended QP map.",
        { phase, sequence: workerTrace.sequence, reason: workerTrace.roiReason,
          changedCtbs, totalCtbs });
    }
    if (changedCtbs * 2 >= totalCtbs) {
      throw new StreamLabUnavailable("quality-roi-coverage-invalid",
        "The exact quality frame QP-map coverage must remain strictly below half the frame.",
        { phase, sequence: workerTrace.sequence, changedCtbs, totalCtbs });
    }
    roiActivity = {
      sequence: workerTrace.sequence >>> 0,
      reason: workerTrace.roiReason,
      changedCtbs,
      totalCtbs,
      coverage: changedCtbs / totalCtbs,
    };
  }
  let hybridActivity = null;
  const hybridRequirement = options.hybridRequirement ??
    (options.requireHybridTiles === true ? "tile" : "off");
  if (!["off", "safe", "tile", "guard"].includes(hybridRequirement))
    throw new RangeError("hybridRequirement must be off, safe, tile, or guard");
  if (hybridRequirement !== "off") {
    const tilesDrawn = Number(workerTrace.hybridTilesDrawn);
    const cropRevision = Number(workerTrace.hybridCropRevision);
    if (!Number.isSafeInteger(tilesDrawn) || !Number.isSafeInteger(cropRevision)) {
      throw new StreamLabUnavailable("quality-hybrid-tile-missing",
        "The exact quality frame did not report generation-bound hybrid activity.",
        { phase, sequence: workerTrace.sequence, tilesDrawn, cropRevision });
    }
    const hybridDiagnostics = diagnostics?.wssMedia?.hybrid ??
      diagnostics?.worker?.hybrid ?? diagnostics?.hybrid ?? {};
    const staleComposites = Number(hybridDiagnostics.staleComposites);
    if (Number.isFinite(staleComposites) && staleComposites !== 0) {
      throw new StreamLabUnavailable("quality-hybrid-stale-composite",
        "The exact quality frame observed a stale hybrid composite.",
        { phase, sequence: workerTrace.sequence,
          staleComposites });
    }
    if (hybridRequirement === "tile" && tilesDrawn <= 0) {
      throw new StreamLabUnavailable("quality-hybrid-tile-missing",
        "The exact quality frame did not composite a matching lossless detail tile.",
        { phase, sequence: workerTrace.sequence, tilesDrawn, cropRevision });
    }
    if (hybridRequirement === "guard" && tilesDrawn !== 0) {
      throw new StreamLabUnavailable("quality-hybrid-motion-overlay",
        "A motion guard sample must present the base video without enhancement overlays.",
        { phase, sequence: workerTrace.sequence, tilesDrawn, cropRevision });
    }
    hybridActivity = {
      sequence: workerTrace.sequence >>> 0,
      cropRevision,
      tilesDrawn,
      ...(options.hybridRequirement ? { requirement: hybridRequirement } : {}),
    };
  }
  const traceDiagnostics = diagnostics?.worker?.frameTrace
    ? { ...diagnostics, worker: { ...diagnostics.worker, frameTrace: workerTrace } }
    : { ...diagnostics, frameTrace: workerTrace };
  return {
    identity: {
      generation: Number.isSafeInteger(source?.generation) ? source.generation : null,
      scene: source?.scene ?? null,
      seed: Number.isSafeInteger(source?.seed) ? source.seed : null,
      frameId: sourceFrameId,
      sequence,
      timestamp,
    },
    roiActivity,
    hybridActivity,
    trace: frameStageTrace(traceDiagnostics, phase, {
      generation: source?.generation,
      sourceFrameId,
      capturedAtEpochMs: sourceTimestamp,
    }),
  };
}

export function performanceFrameTrace(source, diagnostics) {
  try {
    return exactFrameAlignment(source, diagnostics, "performance");
  } catch {
    const trace = frameStageTrace(diagnostics, "performance-unmatched");
    const hasReceiverStage = [
      trace.network.sequence,
      trace.network.receivedAtMs,
      trace.decode.queuedAtMs,
      trace.decode.decodedAtMs,
      trace.presentation.presentedAtMs,
    ].some(Number.isFinite);
    return hasReceiverStage ? { identity: null, trace } : null;
  }
}

export function buildTemporalCapturePlan(sceneIds, options = {}) {
  const maxSamples = options.maxSamples ?? 12;
  if (!Number.isSafeInteger(maxSamples) || maxSamples < 1 || maxSamples > 120) throw new RangeError("maxSamples must be an integer from 1 to 120");
  const framesByScene = {
    "localized-change": options.deltaMode === true ? [59, 89] : [60, 90],
    "scrolling": [0, 30, 60, 90],
    "motion-60": [0, 30, 60, 90],
    "chrome-canvas": [0, 30, 60, 90],
    "generated-video": [0, 30, 60, 90],
  };
  const workloadFor = scene => HYBRID_SCENES.includes(scene) ?
    (scene === "source-code" ? "source-code-fixture" : scene) : "synthetic";
  const perScene = [...new Set(sceneIds)].map(scene =>
    (framesByScene[scene] ?? [0]).map(frameId => ({ scene, frameId, workload: workloadFor(scene) })));
  const selected = [];
  let frameIndex = 0;
  while (selected.length < maxSamples) {
    let added = false;
    for (const samples of perScene) {
      if (samples[frameIndex] && selected.length < maxSamples) { selected.push(samples[frameIndex]); added = true; }
    }
    if (!added) break;
    frameIndex++;
  }
  return selected;
}


export function finiteOrNull(value) { const number = Number(value); return Number.isFinite(number) ? number : null; }

export function frameCount(sample) {
  return Number(sample.framesDrawn ?? sample.nativePresentation?.framesPresented ?? sample.presented) || 0;
}
