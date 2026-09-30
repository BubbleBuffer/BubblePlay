import { STREAM_LAB_COMPATIBILITY } from "../public/media.js";

export function validWssTopology(topology) {
  if (!topology || typeof topology !== "object" || Array.isArray(topology)) return false;
  if (Object.keys(topology).some(key => !["laneCount", "dataShards", "parityShards"].includes(key))) return false;
  const { laneCount, dataShards, parityShards } = topology;
  return Number.isInteger(laneCount) && laneCount >= 1 && laneCount <= 16 &&
    Number.isInteger(dataShards) && dataShards >= 1 && dataShards <= 16 &&
    Number.isInteger(parityShards) && parityShards >= 0 && parityShards <= 2 &&
    laneCount === dataShards + parityShards;
}

export function validCurrentMediaEpoch(epoch, attachment) {
  return typeof epoch === "string" && /^[a-f0-9]{32}$/.test(epoch) && epoch === attachment.mediaEpoch;
}

function uint32(value) { return Number.isInteger(value) && value >= 0 && value <= 0xffff_ffff; }

export function validMediaAckV3(message, attachment) {
  if (message.protocol !== 3 || !validCurrentMediaEpoch(message.epoch, attachment) ||
      !validWssTopology(message.topology) || !Array.isArray(message.lanes) ||
      message.lanes.length !== message.topology.laneCount || !message.lanes.every(uint32)) return false;
  if (message.completionMissingByLane !== undefined &&
      (!Array.isArray(message.completionMissingByLane) ||
       message.completionMissingByLane.length !== message.topology.laneCount ||
       !message.completionMissingByLane.every(uint32))) return false;
  for (const field of ["generation", "reconstructed", "presented", "recoveredFrames", "expiredFrames",
    "decoderDrops", "dependencyLosses", "deadlineMs", "reassemblyP95Ms"]) if (!uint32(message[field])) return false;
  return ["healthy", "awaiting-keyframe", "awaiting-keyframe-output"].includes(message.recoveryState);
}

export function validHostMediaCapabilities(message) {
  const fields = [message.mediaProtocols, message.maxWssLanes, message.wssFec];
  if (fields.every(value => value === undefined)) return true;
  const validFec = Array.isArray(message.wssFec) &&
    (message.wssFec.length === 2 || message.wssFec.length === 3) &&
    message.wssFec[0] === "none" && message.wssFec[1] === "xor-1" &&
    (message.wssFec.length === 2 || message.wssFec[2] === "rs-2");
  // Protocol 4 (single-socket WSS) is an optional, additive capability.
  const protocols = message.mediaProtocols;
  const validProtocols = Array.isArray(protocols) &&
    (protocols.length === 2 || protocols.length === 3) &&
    protocols[0] === 2 && protocols[1] === 3 && (protocols.length === 2 || protocols[2] === 4);
  return validProtocols && message.maxWssLanes === 16 && validFec;
}

export function validInputStatus(message, role, raw) {
  if (role !== "host" || typeof raw !== "string" || utf8Length(raw) > 1_024 ||
      message.protocol !== 1 || !uint32(message.generation)) return false;
  const counters = ["receivedPackets", "acceptedPackets", "failedPackets", "droppedPackets",
    "releasedInputs", "requestedEvents", "appliedEvents"];
  if (!counters.every(field => Number.isSafeInteger(message[field]) && message[field] >= 0)) return false;
  if (message.acceptedPackets > message.receivedPackets ||
      message.failedPackets > message.receivedPackets ||
      message.droppedPackets > message.receivedPackets ||
      message.appliedEvents > message.requestedEvents) return false;
  if (message.lastErrorCode !== undefined &&
      (!Number.isInteger(message.lastErrorCode) || message.lastErrorCode < -0x8000_0000 ||
       message.lastErrorCode > 0x7fff_ffff)) return false;
  return message.lastSequence === undefined ||
    (Number.isInteger(message.lastSequence) && message.lastSequence >= 0 && message.lastSequence <= 0xffff);
}


export function canonicalWorkerVersionId(value) {
  return typeof value === "string" &&
    /^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(value);
}

export function validFirstFrameMessage(msg, role, raw) {
  if (typeof raw !== "string" || utf8Length(raw) > 1_024 || msg.protocol !== 1 ||
      typeof msg.requestId !== "string" || !/^[A-Za-z0-9_-]{8,64}$/.test(msg.requestId) ||
      !["request", "admitted", "presented", "ready", "failed", "quality-keyframe-admitted"].includes(msg.phase)) return false;
  if (msg.phase === "quality-keyframe-admitted") {
    return role === "host" &&
      /^quality-[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(msg.requestId) &&
      msg.transport === "wss" &&
      Number.isInteger(msg.generation) && msg.generation >= 0 && msg.generation <= 0xffffffff &&
      Number.isInteger(msg.sequence) && msg.sequence >= 0 && msg.sequence <= 0xffffffff &&
      ["hevc", "h264"].includes(msg.codec) &&
      msg.reason === "stream-lab-quality-capture";
  }
  if (msg.phase === "request") {
    if (role !== "client" || !["webrtc", "wss"].includes(msg.transport)) return false;
    if (msg.epoch !== undefined && (typeof msg.epoch !== "string" || !/^[a-f0-9]{32}$/.test(msg.epoch))) return false;
    return true;
  }
  if (msg.phase === "presented") {
    return role === "client" && Number.isInteger(msg.generation) && msg.generation >= 0 && msg.generation <= 0xffffffff;
  }
  if (role !== "host" || !Number.isInteger(msg.generation) || msg.generation < 0 || msg.generation > 0xffffffff) return false;
  if (msg.phase === "failed") return typeof msg.reason === "string" && msg.reason.length > 0 && msg.reason.length <= 96;
  return msg.phase === "admitted" || msg.phase === "ready";
}

export function benchStatusRejectionReason(msg, role, raw) {
  const encoder = msg?.effective?.encoder;
  const expandedRoiMode = encoder?.roiMode;
  const normalizedRoiMode = expandedRoiMode ?? (encoder?.dirtyQpMap === true ? "damage" : "off");
  if (role !== "host" || typeof raw !== "string" || utf8Length(raw) > 4_096)
    return "role-or-wire";
  if (![1, 2].includes(msg.protocol)) return "protocol";
  if (!/^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(msg.runId || ""))
    return "run-id";
  if (!Number.isSafeInteger(msg.sequence) || msg.sequence < 0 ||
      !Number.isSafeInteger(msg.profileRevision) || msg.profileRevision < 0)
    return "sequence-or-revision";
  if (!["applied", "sample", "reverted", "error"].includes(msg.phase)) return "phase";
  if (msg.protocol !== 1 && !validBenchExperiment(msg)) return "experiment";
  if (!validBenchEffective(msg.effective, msg.protocol)) return "effective-profile";
  if (msg.protocol !== 1 && (msg.effective.implementationVariant !== msg.implementationVariant ||
      msg.effective.experimentId !== msg.experimentId))
    return "experiment-binding";
  if (msg.roiActivity !== undefined &&
      !(msg.protocol === 2 && normalizedRoiMode !== "off" &&
        validBenchRoiActivity(msg.roiActivity, normalizedRoiMode)))
    return "roi-activity";
  if (expandedRoiMode !== undefined && normalizedRoiMode !== "off" && msg.roiActivity === undefined)
    return "roi-activity-required";
  if (msg.encodeStages !== undefined &&
      !(msg.protocol === 2 && validBenchEncodeStages(msg.encodeStages)))
    return "encode-stages";
  if (msg.hybridActivity === undefined &&
      ["png-quiescent-detail", "hevc-quiescent-tiles"].includes(msg.effective?.hybridMode))
    return "hybrid-activity-required";
  if (msg.hybridActivity !== undefined &&
      !(msg.protocol === 2 && validBenchHybridActivity(msg.hybridActivity, msg.effective?.hybridMode)))
    return "hybrid-activity";
  if (msg.metrics !== undefined &&
      (msg.metrics === null || typeof msg.metrics !== "object" || Array.isArray(msg.metrics)))
    return "metrics";
  return null;
}

function validBenchStatus(msg, role, raw) {
  return benchStatusRejectionReason(msg, role, raw) === null;
}

function validBenchRoiActivity(value, expectedMode) {
  const required = ["mode", "evaluatedFrames", "mappedFrames", "missingDamageFrames", "emptyDamageFrames",
    "idrFrames", "coverageRejectedFrames", "unsupportedFrames"];
  const optional = ["source", "temporalFrames", "temporalPrimingFrames", "analysisP95Us", "analysisMaxUs",
    "lastSequence", "lastReason", "lastChangedCtbs", "lastTotalCtbs", "lastMinDelta", "lastMaxDelta",
    "reconstructionActive", "feedbackSubmittedFrames", "feedbackReadyFrames", "feedbackLateFrames",
    "feedbackSkippedFrames", "feedbackAdjustedFrames", "feedbackWarmupFrames",
    "feedbackMotionBypasses", "feedbackMissingFrames", "feedbackEligibleCtbs",
    "feedbackRefinedCtbs", "feedbackFundedCtbs", "feedbackSubmitP95Us",
    "feedbackSubmitMaxUs", "feedbackReadbackP95Us", "feedbackReadbackMaxUs",
    "lastFeedbackSequence", "lastFeedbackAgeFrames", "feedbackSpendHeadroomFrames",
    "feedbackNetZeroFrames", "feedbackWireOccupancyP95Bps", "feedbackWireOccupancyMaxBps",
    "lastFeedbackBudgetPolicy", "lastFeedbackWireOccupancyBps"];
  if (value === null || typeof value !== "object" || Array.isArray(value) ||
      !required.every(key => Object.hasOwn(value, key)) ||
      !Object.keys(value).every(key => required.includes(key) || optional.includes(key)) ||
      value.mode !== expectedMode) return false;
  const countKeys = required.slice(1);
  if (!countKeys.every(key => Number.isSafeInteger(value[key]) && value[key] >= 0)) return false;
  if (value.evaluatedFrames !== countKeys.slice(1).reduce((sum, key) => sum + value[key], 0)) return false;
  if (value.lastSequence !== undefined && !boundedInteger(value.lastSequence, 0, 0xffff_ffff)) return false;
  if (value.lastReason !== undefined && !["applied", "missing-damage", "empty-damage", "idr",
    "coverage-at-least-half", "unsupported-encoder-state"].includes(value.lastReason)) return false;
  if (value.lastChangedCtbs !== undefined && !boundedInteger(value.lastChangedCtbs, 0, 8_160)) return false;
  if (value.lastTotalCtbs !== undefined && !boundedInteger(value.lastTotalCtbs, 0, 8_160)) return false;
  if (value.lastChangedCtbs !== undefined && value.lastTotalCtbs !== undefined &&
      value.lastChangedCtbs > value.lastTotalCtbs) return false;
  if (value.lastMinDelta !== undefined && !boundedInteger(value.lastMinDelta, -5, 2)) return false;
  if (value.lastMaxDelta !== undefined && !boundedInteger(value.lastMaxDelta, -5, 2)) return false;
  if (value.source !== undefined && !["dda", "gpu-temporal"].includes(value.source)) return false;
  if (value.temporalFrames !== undefined && !boundedInteger(value.temporalFrames, 0, 1_000_000)) return false;
  if (value.temporalPrimingFrames !== undefined &&
      !boundedInteger(value.temporalPrimingFrames, 0, 1_000_000)) return false;
  if (value.temporalPrimingFrames !== undefined && value.temporalFrames !== undefined &&
      value.temporalPrimingFrames > value.temporalFrames) return false;
  if (value.analysisP95Us !== undefined && !boundedInteger(value.analysisP95Us, 0, 60_000_000)) return false;
  if (value.analysisMaxUs !== undefined && !boundedInteger(value.analysisMaxUs, 0, 60_000_000)) return false;
  if (value.analysisP95Us !== undefined && value.analysisMaxUs !== undefined &&
      value.analysisP95Us > value.analysisMaxUs) return false;
  const temporalKeysPresent = ["temporalFrames", "temporalPrimingFrames", "analysisP95Us", "analysisMaxUs"]
    .some(key => value[key] !== undefined);
  if (temporalKeysPresent && value.source !== "gpu-temporal") return false;
  if (value.source === "gpu-temporal" &&
      (!Number.isSafeInteger(value.temporalFrames) || value.temporalFrames <= 0 ||
       value.analysisP95Us === undefined || value.analysisMaxUs === undefined)) return false;
  const feedbackCountKeys = ["feedbackSubmittedFrames", "feedbackReadyFrames", "feedbackLateFrames",
    "feedbackSkippedFrames", "feedbackAdjustedFrames", "feedbackWarmupFrames",
    "feedbackMotionBypasses", "feedbackMissingFrames", "feedbackEligibleCtbs",
    "feedbackRefinedCtbs", "feedbackFundedCtbs"];
  const feedbackTimingKeys = ["feedbackSubmitP95Us", "feedbackSubmitMaxUs",
    "feedbackReadbackP95Us", "feedbackReadbackMaxUs"];
  const feedbackKeys = ["reconstructionActive", ...feedbackCountKeys, ...feedbackTimingKeys];
  const feedbackLastKeys = ["lastFeedbackSequence", "lastFeedbackAgeFrames"];
  const feedbackMode = ["feedback", "feedback-headroom"].includes(expectedMode);
  const feedbackKeysPresent = [...feedbackKeys, ...feedbackLastKeys]
    .some(key => value[key] !== undefined);
  if (feedbackKeysPresent !== feedbackMode) return false;
  if (feedbackMode) {
    if (!feedbackKeys.every(key => Object.hasOwn(value, key)) ||
        typeof value.reconstructionActive !== "boolean")
      return false;
    if (!feedbackCountKeys.every(key => boundedInteger(value[key], 0, 1_000_000))) return false;
    if (!feedbackTimingKeys.every(key => boundedInteger(value[key], 0, 60_000_000))) return false;
    const lastKeysPresent = feedbackLastKeys.filter(key => value[key] !== undefined).length;
    if (lastKeysPresent !== 0 && lastKeysPresent !== feedbackLastKeys.length) return false;
    if (lastKeysPresent && (!boundedInteger(value.lastFeedbackSequence, 0, 0xffff_ffff) ||
        value.lastFeedbackAgeFrames !== 1)) return false;
    if (value.feedbackAdjustedFrames !== value.mappedFrames ||
        value.feedbackSubmitP95Us > value.feedbackSubmitMaxUs ||
        value.feedbackReadbackP95Us > value.feedbackReadbackMaxUs) return false;
    if (expectedMode === "feedback" && value.feedbackFundedCtbs !== value.feedbackRefinedCtbs * 2)
      return false;
  }
  const headroomCountKeys = ["feedbackSpendHeadroomFrames", "feedbackNetZeroFrames"];
  const headroomOccupancyKeys = ["feedbackWireOccupancyP95Bps", "feedbackWireOccupancyMaxBps"];
  const headroomLastKeys = ["lastFeedbackBudgetPolicy", "lastFeedbackWireOccupancyBps"];
  const headroomKeys = [...headroomCountKeys, ...headroomOccupancyKeys, ...headroomLastKeys];
  const headroomKeysPresent = headroomKeys.filter(key => value[key] !== undefined).length;
  if (expectedMode === "feedback-headroom") {
    if (![headroomKeys.length - headroomLastKeys.length, headroomKeys.length].includes(headroomKeysPresent) ||
        !headroomCountKeys.every(key => boundedInteger(value[key], 0, 1_000_000)) ||
        !headroomOccupancyKeys.every(key => boundedInteger(value[key], 0, 1_000_000_000)) ||
        value.feedbackSpendHeadroomFrames + value.feedbackNetZeroFrames !== value.feedbackAdjustedFrames ||
        value.feedbackWireOccupancyP95Bps > value.feedbackWireOccupancyMaxBps)
      return false;
    const headroomLastPresent = headroomLastKeys.filter(key => value[key] !== undefined).length;
    if (headroomLastPresent !== 0 && headroomLastPresent !== headroomLastKeys.length) return false;
    if (headroomLastPresent &&
        (!['net-zero', 'spend-headroom'].includes(value.lastFeedbackBudgetPolicy) ||
         !boundedInteger(value.lastFeedbackWireOccupancyBps, 0, 1_000_000_000))) return false;
  } else if (headroomKeysPresent !== 0) {
    return false;
  }
  return (value.lastMinDelta === undefined || value.lastMaxDelta === undefined ||
    value.lastMinDelta <= value.lastMaxDelta);
}

function validBenchEncodeStages(value) {
  const stageNames = ["register", "map", "submit", "lock", "copy", "unmap", "unregister"];
  if (!exactObjectKeys(value, ["frames", ...stageNames]) ||
      !Number.isSafeInteger(value.frames) || value.frames < 0) return false;
  for (const name of stageNames) {
    const stage = value[name];
    if (!exactObjectKeys(stage, ["count", "totalUs", "maxUs"]) ||
        !Number.isSafeInteger(stage.count) || stage.count < 0 ||
        !Number.isSafeInteger(stage.totalUs) || stage.totalUs < 0 ||
        !Number.isSafeInteger(stage.maxUs) || stage.maxUs < 0 ||
        stage.maxUs > stage.totalUs ||
        (stage.count === 0 && (stage.totalUs !== 0 || stage.maxUs !== 0))) return false;
  }
  if (!["map", "submit", "lock", "copy", "unmap"].every(name => value[name].count === value.frames))
    return false;
  return value.register.count <= value.frames && value.unregister.count <= value.frames;
}

function validBenchHybridActivity(value, expectedMode) {
  const countKeys = ["frames", "historyResets", "historyPreservedFrames", "motionBypasses",
    "cropRemaps", "cursorNeutralFrames", "candidates", "reissuedTiles", "upserts",
    "invalidations", "budgetSkippedUpserts", "pngBytes", "pngUsTotal", "pngUsMax",
    "sidecarBytes", "deferredValidationReadyFrames", "deferredValidationPendingFrames",
    "damageAuditFrames", "damageAuditMetadataUnavailableFrames", "damageAuditChangedCtbs",
    "damageAuditCoveredCtbs", "damageAuditMissedCtbs"];
  const validationCounts = ["validationFrames", "validationUnavailableFrames", "validationFailedFrames"];
  const optionalZeroCounts = ["validationUnavailableFrames", "validationFailedFrames",
    "damageAuditMetadataUnavailableFrames", "damageAuditMissedCtbs"];
  const validationTimings = ["validationSubmitP95Us", "validationSubmitMaxUs",
    "validationCollectP95Us", "validationCollectMaxUs"];
  const deferredAges = ["deferredValidationAgeP95Frames", "deferredValidationAgeMaxFrames",
    "deferredValidationPendingMaxAgeFrames"];
  const hevcTileCounts = ["hevcTileBytes", "hevcTileCopyUsTotal", "hevcTileCopyUsMax",
    "hevcTileEncodeUsTotal", "hevcTileEncodeUsMax", "hevcTileEncoderOpens",
    "hevcTileEncoderFailures"];
  const optional = ["analysisP95Us", "analysisMaxUs", ...validationCounts, ...validationTimings,
    "lastValidationSequence", ...deferredAges, ...hevcTileCounts];
  if (!exactObjectKeys(value, ["mode", "cache",
    ...countKeys.filter(key => !optionalZeroCounts.includes(key)),
    ...optionalZeroCounts.filter(key => value?.[key] !== undefined),
    ...optional.filter(key => !optionalZeroCounts.includes(key) && value?.[key] !== undefined)]) ||
      !["png-quiescent-detail", "hevc-quiescent-tiles"].includes(expectedMode) ||
      value.mode !== expectedMode ||
      !countKeys.every(key => Number.isSafeInteger(value[key] ?? 0) && (value[key] ?? 0) >= 0)) return false;
  if (!hevcTileCounts.every(key => value[key] === undefined ||
      (Number.isSafeInteger(value[key]) && value[key] >= 0))) return false;
  if (!exactObjectKeys(value.cache, ["s", "r", "i", "x", "b", "a"]) ||
      !["s", "r", "i", "x", "b", "a"].every(
        key => Number.isSafeInteger(value.cache[key]) && value.cache[key] >= 0)) return false;
  if (value.pngUsMax > value.pngUsTotal) return false;
  if ((value.hevcTileCopyUsMax ?? 0) > (value.hevcTileCopyUsTotal ?? 0) ||
      (value.hevcTileEncodeUsMax ?? 0) > (value.hevcTileEncodeUsTotal ?? 0)) return false;
  if (value.analysisP95Us !== undefined &&
      !boundedInteger(value.analysisP95Us, 0, 60_000_000)) return false;
  if (value.analysisMaxUs !== undefined &&
      !boundedInteger(value.analysisMaxUs, 0, 60_000_000)) return false;
  if (value.analysisP95Us !== undefined && value.analysisMaxUs !== undefined &&
      value.analysisP95Us > value.analysisMaxUs) return false;
  const validationPresent = optional.slice(2).some(key => value[key] !== undefined);
  if (!validationPresent) return true;
  if (!validationCounts.every(key => boundedInteger(value[key] ?? 0, 0, 1_000_000))) return false;
  if ((value.validationUnavailableFrames ?? 0) + (value.validationFailedFrames ?? 0) >
      value.validationFrames)
    return false;
  if (value.lastValidationSequence !== undefined &&
      !boundedInteger(value.lastValidationSequence, 0, 0xffff_ffff)) return false;
  for (const key of validationTimings) {
    if (value[key] !== undefined && !boundedInteger(value[key], 0, 60_000_000)) return false;
  }
  if (value.validationFrames > 0 && !validationTimings.every(key => value[key] !== undefined))
    return false;
  if (!deferredAges.every(key => value[key] === undefined || boundedInteger(value[key], 0, 1_000_000)))
    return false;
  if (value.deferredValidationAgeP95Frames !== undefined &&
      value.deferredValidationAgeMaxFrames !== undefined &&
      value.deferredValidationAgeP95Frames > value.deferredValidationAgeMaxFrames) return false;
  if ((value.damageAuditMetadataUnavailableFrames ?? 0) > value.damageAuditFrames ||
      value.damageAuditCoveredCtbs > value.damageAuditChangedCtbs ||
      (value.damageAuditMissedCtbs ?? 0) > value.damageAuditChangedCtbs ||
      value.damageAuditCoveredCtbs + (value.damageAuditMissedCtbs ?? 0) !==
        value.damageAuditChangedCtbs)
    return false;
  return (value.validationSubmitP95Us === undefined || value.validationSubmitMaxUs === undefined ||
      value.validationSubmitP95Us <= value.validationSubmitMaxUs) &&
    (value.validationCollectP95Us === undefined || value.validationCollectMaxUs === undefined ||
      value.validationCollectP95Us <= value.validationCollectMaxUs);
}

function validBenchExperiment(value) {
  return ["production", "candidate"].includes(value.implementationVariant) &&
    typeof value.experimentId === "string" && /^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$/.test(value.experimentId);
}

function validBenchEffective(effective, protocol = 1) {
  const keys = ["transport", "codec", "streamMode", "width", "height", "fps", "bitrateBps", "gopSeconds", "encoder"];
  if (protocol === 2) {
    keys.push("implementationVariant", "experimentId");
    if (effective?.wssTopology !== undefined) keys.push("wssTopology");
    if (effective?.hybridMode !== undefined) keys.push("hybridMode");
    if (effective?.hybridCacheMode !== undefined) keys.push("hybridCacheMode");
  }
  if (!exactObjectKeys(effective, keys)) return false;
  const maxFps = effective.streamMode === "work" ? 30 : effective.streamMode === "game" ? 60 : 0;
  // The v2 experiment search remains 2–12 Mbps, while a shaped lab unit may
  // publish a lower effective target so parity and framing fit under its
  // total browser-delivered media cap.
  const bitrateBounds = protocol === 2 ? [500_000, 12_000_000] : [500_000, 40_000_000];
  return ["wss", "webrtc"].includes(effective.transport) && effective.codec === "hevc" && maxFps > 0 &&
    boundedInteger(effective.width, 320, 3_840) && boundedInteger(effective.height, 180, 2_160) &&
    boundedInteger(effective.fps, 1, maxFps) && boundedInteger(effective.bitrateBps, ...bitrateBounds) &&
    [1, 2, 4].includes(effective.gopSeconds) && (protocol === 1 || validBenchExperiment(effective)) &&
    (effective.wssTopology === undefined || (effective.transport === "wss" && validWssTopology(effective.wssTopology))) &&
    validBenchEncoder(effective.encoder, protocol, effective.streamMode) &&
    validBenchPipelineControls(effective, protocol);
}

function validBenchPipelineControls(effective, protocol) {
  if (protocol !== 2) return true;
  const encoder = effective.encoder;
  const hybridCacheMode = effective.hybridCacheMode ?? "suppress-reactivate";
  const expanded = effective.hybridMode !== undefined || encoder.roiMode !== undefined ||
    encoder.resourceMode !== undefined || encoder.asyncDepth !== undefined ||
    effective.hybridCacheMode !== undefined || encoder.rateControlMode !== undefined;
  if (!expanded) return true;
  if (!["off", "png-static-roi", "png-quiescent-detail", "hevc-quiescent-tiles"].includes(
    effective.hybridMode) ||
      !["dda-gated", "suppress-reactivate"].includes(hybridCacheMode) ||
      !["off", "damage", "text-edge", "feedback", "feedback-headroom"].includes(encoder.roiMode) ||
      !["per-frame", "persistent"].includes(encoder.resourceMode) ||
      ![0, 1].includes(encoder.asyncDepth)) return false;
  const rateControlMode = encoder.rateControlMode ?? "cbr";
  if (!["cbr", "external-budget-vbr"].includes(rateControlMode) ||
      (rateControlMode === "cbr" &&
       (encoder.targetQuality !== undefined || encoder.peakBitrateMultiplier !== undefined)) ||
      (rateControlMode === "external-budget-vbr" &&
       (encoder.targetQuality !== 23 || encoder.peakBitrateMultiplier !== 4 || encoder.vbvFrames !== 4)))
    return false;
  if (hybridCacheMode === "dda-gated" && effective.hybridMode !== "png-quiescent-detail")
    return false;
  const activeAxes = Number(encoder.roiMode !== "off") + Number(effective.hybridMode !== "off") +
    Number(encoder.resourceMode !== "per-frame" || encoder.asyncDepth !== 0) +
    Number(rateControlMode !== "cbr");
  if (activeAxes === 0) return true;
  const topology = effective.wssTopology;
  return activeAxes === 1 && effective.implementationVariant === "candidate" &&
    effective.transport === "wss" && effective.streamMode === "work" &&
    effective.width === 1920 && effective.height === 1080 && effective.fps === 30 &&
    boundedInteger(effective.bitrateBps, 500_000, 6_000_000) &&
    topology?.laneCount === 16 && topology.dataShards === 15 && topology.parityShards === 1 &&
    encoder.main10 === false && (encoder.asyncDepth === 0 || encoder.resourceMode === "persistent");
}

function validBenchEncoder(encoder, protocol = 1, streamMode = "work") {
  if (protocol === 2) {
    const legacyKeys = ["preset", "tuning", "multipass", "aqMode", "aqStrength", "vbvFrames", "lookaheadDepth", "weightedPrediction", "dirtyQpMap", "main10"];
    const expandedKeys = [...legacyKeys, "roiMode", "resourceMode", "asyncDepth"];
    const rateControlKeys = [...expandedKeys, "rateControlMode", "targetQuality", "peakBitrateMultiplier"];
    const shapeValid = exactObjectKeys(encoder, legacyKeys) || exactObjectKeys(encoder, expandedKeys) ||
      exactObjectKeys(encoder, rateControlKeys);
    return shapeValid &&
      ["p1", "p2", "p3", "p4", "p5", "p6", "p7"].includes(encoder.preset) &&
      ["ultra-low-latency", "low-latency"].includes(encoder.tuning) &&
      ["disabled", "quarter", "full"].includes(encoder.multipass) &&
      ["none", "spatial", "temporal"].includes(encoder.aqMode) &&
      boundedInteger(encoder.aqStrength, 1, 15) && boundedInteger(encoder.vbvFrames, 1, 4) &&
      boundedInteger(encoder.lookaheadDepth, 0, streamMode === "game" ? 1 : 2) &&
      typeof encoder.weightedPrediction === "boolean" && typeof encoder.dirtyQpMap === "boolean" &&
      typeof encoder.main10 === "boolean" &&
      (encoder.roiMode === undefined || ["off", "damage", "text-edge", "feedback", "feedback-headroom"].includes(encoder.roiMode)) &&
      (encoder.resourceMode === undefined || ["per-frame", "persistent"].includes(encoder.resourceMode)) &&
      (encoder.asyncDepth === undefined || [0, 1].includes(encoder.asyncDepth)) &&
      (encoder.rateControlMode === undefined || encoder.rateControlMode === "external-budget-vbr") &&
      (encoder.rateControlMode === undefined ||
       (encoder.targetQuality === 23 && encoder.peakBitrateMultiplier === 4 && encoder.vbvFrames === 4)) &&
      !(encoder.dirtyQpMap && encoder.roiMode !== undefined && encoder.roiMode !== "damage");
  }
  return exactObjectKeys(encoder, ["preset", "tuning", "multipass", "aqStrength", "temporalAq", "vbvFrames"]) &&
    ["p1", "p3", "p4", "p5"].includes(encoder.preset) &&
    ["ultra-low-latency", "low-latency"].includes(encoder.tuning) &&
    ["disabled", "quarter"].includes(encoder.multipass) &&
    boundedInteger(encoder.aqStrength, 1, 15) && typeof encoder.temporalAq === "boolean" &&
    boundedInteger(encoder.vbvFrames, 1, 4);
}

function exactObjectKeys(value, expected) {
  if (value === null || typeof value !== "object" || Array.isArray(value)) return false;
  const keys = Object.keys(value);
  return keys.length === expected.length && expected.every(key => Object.hasOwn(value, key));
}

function boundedInteger(value, minimum, maximum) {
  return Number.isInteger(value) && value >= minimum && value <= maximum;
}

// A distinct Durable Object instance keeps high-frequency media forwarding
// off the signaling actor. The protocol implementation is shared, but storage,
// socket membership, hibernation and failure lifecycle are isolated.

export function utf8Length(value) {
  return new TextEncoder().encode(value).byteLength;
}

