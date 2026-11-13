// Small, browser-and-Node compatible state machine for the receive pipeline.
// WebCodecs timestamps are defined in microseconds; keeping that unit here
// avoids silently treating RTP's 90 kHz clock as a wall clock.
export function createLatencyGuard({ now = () => performance.now(), onReset = () => {}, startupGraceMs = 1000 } = {}) {
  let generation = 0;
  let workerFrames = 0;
  let decodeFrames = 0;
  let anchorTimestamp = null;
  let anchorWall = 0;
  let lastTimestamp = null;
  let lastWall = null;
  let consecutiveLate = 0;
  let graceUntil = 0;

  function reset(reason = "resync", requestedGeneration) {
    if (Number.isInteger(requestedGeneration)) {
      // Worker notifications can race with a newer main-thread reset.
      if (requestedGeneration <= generation) return;
      generation = requestedGeneration;
    } else {
      generation++;
    }
    workerFrames = 0;
    decodeFrames = 0;
    anchorTimestamp = null;
    anchorWall = 0;
    lastTimestamp = null;
    lastWall = null;
    consecutiveLate = 0;
    graceUntil = now() + startupGraceMs;
    onReset({ generation, reason });
  }

  function acceptWorkerFrame(frame) {
    if (frame.generation !== generation) return false;
    if (workerFrames >= 2) {
      reset("main-worker-overflow");
      return false;
    }
    const timestamp = frame.timestamp;
    const wall = now();
    if (!Number.isFinite(timestamp)) {
      reset("invalid-timestamp");
      return false;
    }
    if (anchorTimestamp === null) {
      anchorTimestamp = timestamp;
      anchorWall = wall;
      graceUntil = wall + startupGraceMs;
    } else {
      const delta = timestamp - lastTimestamp;
      const mediaGapMs = delta / 1000;
      const wallGapMs = wall - lastWall;
      // A long capture gap is valid when the desktop was static: host RTP
      // timestamps advance from wall time even though duplicate frames are
      // omitted. Only treat a large forward jump as discontinuous when it did
      // not also happen in wall time.
      if (delta <= 0 ||
          (mediaGapMs > 2000 && Math.abs(mediaGapMs - wallGapMs) > 500)) {
        reset("timestamp-discontinuity");
        return false;
      }
      const expectedWall = anchorWall + (timestamp - anchorTimestamp) / 1000;
      if (wall >= graceUntil && wall - expectedWall > 100) {
        consecutiveLate++;
        // Decode the first two late frames to preserve H.264 reference
        // continuity, but never render them. A sustained backlog resets the
        // whole dependency chain and requests a fresh keyframe.
        if (consecutiveLate >= 3) {
          reset("three-late-frames");
          return false;
        }
      } else {
        consecutiveLate = 0;
      }
    }
    lastTimestamp = timestamp;
    lastWall = wall;
    workerFrames++;
    return true;
  }

  function expectedWall(timestamp) {
    if (anchorTimestamp === null || !Number.isFinite(timestamp)) return null;
    return anchorWall + (timestamp - anchorTimestamp) / 1000;
  }

  function reanchor(timestamp, wall = now()) {
    if (!Number.isFinite(timestamp) || !Number.isFinite(wall)) return false;
    anchorTimestamp = timestamp;
    anchorWall = wall;
    lastTimestamp = timestamp;
    lastWall = wall;
    consecutiveLate = 0;
    graceUntil = wall + startupGraceMs;
    return true;
  }

  return {
    get generation() { return generation; },
    get workerFrames() { return workerFrames; },
    get decodeFrames() { return decodeFrames; },
    acceptWorkerFrame,
    expectedWall,
    reanchor,
    presentationAgeMs(timestamp, at = now()) {
      const expected = expectedWall(timestamp);
      return expected === null ? null : at - expected;
    },
    releaseWorkerFrame() { workerFrames = Math.max(0, workerFrames - 1); },
    admitDecode(limit = 2) {
      if (decodeFrames >= limit) return false;
      decodeFrames++;
      return true;
    },
    releaseDecode() { decodeFrames = Math.max(0, decodeFrames - 1); },
    reset,
  };
}

export function annexBHasIdr(bytes) {
  for (let index = 0; index + 4 < bytes.length; index++) {
    const start3 = bytes[index] === 0 && bytes[index + 1] === 0 && bytes[index + 2] === 1;
    const start4 = bytes[index] === 0 && bytes[index + 1] === 0 && bytes[index + 2] === 0 && bytes[index + 3] === 1;
    if (!start3 && !start4) continue;
    const nal = index + (start4 ? 4 : 3);
    if ((bytes[nal] & 0x1f) === 5) return true;
    index = nal - 1;
  }
  return false;
}

export function normalizeEncodedTimestamp(valueTimestamp, metadata = {}) {
  if (Number.isFinite(metadata.timestamp)) return metadata.timestamp;
  if (Number.isFinite(metadata.rtpTimestamp)) {
    return Math.round(metadata.rtpTimestamp * 1000 / 90);
  }
  // Legacy RTCEncodedVideoFrame.timestamp is the 90 kHz RTP timestamp,
  // unlike metadata.timestamp (microsecond PTS).
  return Number.isFinite(valueTimestamp) ? Math.round(valueTimestamp * 1000 / 90) : NaN;
}

// Convert a browser RTCStatsReport into a compact, JSON-safe absolute
// snapshot. Absolute counters let the host compare any adjacent samples even
// when a signaling message is lost; no browser-only objects cross the wire.
export function summarizeRtcStats(report) {
  let inbound = null;
  let candidatePair = null;
  const transports = [];
  report.forEach((entry) => {
    if (entry.type === "inbound-rtp" &&
        (entry.kind === "video" || entry.mediaType === "video")) inbound = entry;
    if (entry.type === "candidate-pair" && entry.state === "succeeded" &&
        (entry.nominated || entry.selected)) candidatePair = entry;
    if (entry.type === "transport") transports.push(entry);
  });
  // RTCTransportStats identifies the pair actually carrying this video. Its
  // current checklist state need not match the legacy nominated-pair filter.
  const transport = inbound?.transportId ? report.get?.(inbound.transportId)
    : transports.length === 1 ? transports[0] : null;
  if (transport?.selectedCandidatePairId) {
    const selected = report.get?.(transport.selectedCandidatePairId);
    candidatePair = selected?.type === "candidate-pair" ? selected : null;
  }

  const copy = (source, keys) => Object.fromEntries(keys
    .filter((key) => source?.[key] !== undefined && source?.[key] !== null)
    .map((key) => [key, source[key]]));
  const candidate = (id) => copy(report.get?.(id), [
    "candidateType", "protocol", "networkType", "relayProtocol", "tcpType",
  ]);

  const inboundSummary = inbound ? {
    ...copy(inbound, [
      "timestamp", "packetsReceived", "packetsLost", "bytesReceived",
      "framesReceived", "framesDecoded", "framesDropped", "keyFramesDecoded",
      "framesPerSecond", "freezeCount", "pauseCount", "qpSum", "nackCount",
      "pliCount", "firCount", "retransmittedPacketsReceived",
      "jitterBufferEmittedCount", "decoderImplementation", "powerEfficientDecoder",
    ]),
    ...(Number.isFinite(inbound.jitter) ? { jitterMs: Math.round(inbound.jitter * 1000) } : {}),
    ...(Number.isFinite(inbound.jitterBufferDelay) ? { jitterBufferDelayMs: Math.round(inbound.jitterBufferDelay * 1000) } : {}),
    ...(Number.isFinite(inbound.jitterBufferMinimumDelay) ? { jitterBufferMinimumDelayMs: Math.round(inbound.jitterBufferMinimumDelay * 1000) } : {}),
    ...(Number.isFinite(inbound.jitterBufferTargetDelay) ? { jitterBufferTargetDelayMs: Math.round(inbound.jitterBufferTargetDelay * 1000) } : {}),
    ...(Number.isFinite(inbound.totalFreezesDuration) ? { totalFreezesDurationMs: Math.round(inbound.totalFreezesDuration * 1000) } : {}),
    ...(Number.isFinite(inbound.totalDecodeTime) ? { totalDecodeTimeMs: Math.round(inbound.totalDecodeTime * 1000) } : {}),
    ...(Number.isFinite(inbound.totalInterFrameDelay) ? { totalInterFrameDelayMs: Math.round(inbound.totalInterFrameDelay * 1000) } : {}),
  } : null;
  const pairSummary = candidatePair ? {
    ...copy(candidatePair, [
      "state", "nominated", "bytesReceived", "bytesSent", "packetsReceived",
      "packetsSent", "availableIncomingBitrate", "availableOutgoingBitrate",
    ]),
    ...(Number.isFinite(candidatePair.currentRoundTripTime) ?
      { currentRoundTripTimeMs: Math.round(candidatePair.currentRoundTripTime * 1000) } : {}),
    local: candidate(candidatePair.localCandidateId),
    remote: candidate(candidatePair.remoteCandidateId),
  } : null;
  const codec = inbound?.codecId ? copy(report.get?.(inbound.codecId), [
    "mimeType", "clockRate", "channels", "sdpFmtpLine",
  ]) : null;
  return { inbound: inboundSummary, candidatePair: pairSummary, codec,
    transport: transport ? copy(transport, ["iceState", "dtlsState", "selectedCandidatePairId"]) : null };
}

// Return only the receiver telemetry that crosses the adaptive-quality wire.
// `current` and `previous` are adjacent samples from one peer generation;
// counters that decrease are treated as a reset instead of a negative spike.
export function computeQosRxWindow(current = {}, previous = null) {
  const finite = (value) => Number.isFinite(value) ? value : null;
  const counterDelta = (key) => {
    const value = finite(current[key]);
    const before = finite(previous?.[key]);
    if (value === null || before === null || value < 0 || before < 0 || value < before) return null;
    return value - before;
  };
  const sameGeneration = previous && current.generation === previous.generation;
  const elapsedMs = sameGeneration && finite(current.atMs) !== null && finite(previous.atMs) !== null ?
    current.atMs - previous.atMs : null;
  const usableWindow = elapsedMs !== null && elapsedMs > 0;
  const delta = (key) => usableWindow ? counterDelta(key) : null;
  const packetsReceivedDelta = delta("packetsReceived");
  const packetsLostDelta = delta("packetsLost");
  const bytesDelta = delta("bytesReceived");
  const emittedDelta = delta("jitterBufferEmittedCount");
  const delayDelta = delta("jitterBufferDelay");
  const decodedDelta = delta("framesDecoded");
  const qpDelta = delta("qpSum");
  const packetTotal = packetsReceivedDelta === null || packetsLostDelta === null ? null :
    packetsReceivedDelta + packetsLostDelta;
  return {
    mbps: bytesDelta === null ? null : bytesDelta * 8 / elapsedMs / 1_000,
    lossPct: packetTotal ? packetsLostDelta / packetTotal * 100 : null,
    rttMs: finite(current.rtt) === null ? null : Math.round(current.rtt * 1_000),
    jitterMs: finite(current.jitter) === null ? null : Math.round(current.jitter * 1_000),
    jitterBufferMs: delayDelta === null || emittedDelta === null || emittedDelta <= 0 ? null :
      delayDelta / emittedDelta * 1_000,
    nackDelta: delta("nackCount"),
    pliDelta: delta("pliCount"),
    packetsReceivedDelta,
    framesReceivedDelta: delta("framesReceived"),
    framesDecodedDelta: decodedDelta,
    framesDroppedDelta: delta("framesDropped"),
    freezeDelta: delta("freezeCount"),
    qpPerFrame: qpDelta === null || decodedDelta === null || decodedDelta <= 0 ? null : qpDelta / decodedDelta,
  };
}

export function createQosPayload({ sessionId, seq, atMs, rx = {}, presentation = {} }) {
  const finiteOrNull = (value) => Number.isFinite(value) ? value : null;
  const pick = (source, keys) => Object.fromEntries(keys.map((key) => [key, finiteOrNull(source[key])]));
  if (typeof sessionId !== "string" || sessionId.length === 0 || sessionId.length > 128 ||
      !Number.isSafeInteger(seq) || seq < 0 || !Number.isFinite(atMs)) return null;
  return {
    type: "qos",
    sessionId,
    seq,
    atMs,
    rx: pick(rx, [
      "mbps", "lossPct", "rttMs", "jitterMs", "jitterBufferMs", "nackDelta", "pliDelta",
      "packetsReceivedDelta", "framesReceivedDelta", "framesDecodedDelta", "framesDroppedDelta",
      "freezeDelta", "qpPerFrame",
    ]),
    presentation: pick(presentation, ["fps", "recentGapMs", "width", "height"]),
  };
}

// Host quality announcements are untrusted relay input. Keep the accepted
// shape small, bounded, and JSON-safe before exposing it to the HUD/diagnostic
// snapshot.
export function parseHostQuality(message) {
  const boundedInteger = (value, max) => Number.isInteger(value) && value >= 0 && value <= max ? value : null;
  const boundedText = (value, max) => typeof value === "string" && value.length > 0 && value.length <= max &&
    [...value].every((char) => char >= " " && char <= "~") ? value : null;
  if (!message || message.type !== "quality") return null;
  const profile = boundedText(message.profile, 32);
  const width = boundedInteger(message.width, 16_384);
  const height = boundedInteger(message.height, 16_384);
  const fps = boundedInteger(message.fps, 240);
  const targetBitrateBps = boundedInteger(message.targetBitrateBps, 100_000_000);
  const bweBps = message.bweBps === null || message.bweBps === undefined ? null : boundedInteger(message.bweBps, 100_000_000);
  const reason = boundedText(message.reason, 64);
  const invalidBwe = bweBps === null && message.bweBps !== null && message.bweBps !== undefined;
  if (profile === null || !width || !height || !fps || !targetBitrateBps || invalidBwe || reason === null) return null;
  return { profile, width, height, fps, targetBitrateBps, bweBps, reason };
}

// Shared by the production transform worker and Node tests: encoded timing is
// judged on its media timeline, not on whichever thread received a callback.
export function createWorkerTimeline({ now = () => performance.now(), startupGraceMs = 1000 } = {}) {
  let baseTimestamp = null, baseWall = 0, lastTimestamp = null, lastWall = null, late = 0, graceUntil = 0;
  return {
    observe(timestamp) {
      const wall = now();
      if (!Number.isFinite(timestamp)) return { ok: false, reason: "invalid-timestamp" };
      if (baseTimestamp === null) { baseTimestamp = lastTimestamp = timestamp; baseWall = lastWall = wall; graceUntil = wall + startupGraceMs; return { ok: true, due: wall }; }
      const delta = timestamp - lastTimestamp;
      const mediaGapMs = delta / 1000, wallGapMs = wall - lastWall;
      if (delta <= 0 || (mediaGapMs > 2000 && Math.abs(mediaGapMs - wallGapMs) > 500)) return { ok: false, reason: "timestamp-discontinuity" };
      lastTimestamp = timestamp; lastWall = wall; const due = baseWall + (timestamp - baseTimestamp) / 1000;
      if (wall >= graceUntil && wall - due > 100) { late++; if (late >= 3) return { ok: false, reason: "three-late-frames" }; } else late = 0;
      return { ok: true, due };
    },
    reset() { baseTimestamp = null; lastTimestamp = null; lastWall = null; late = 0; },
  };
}

// One decoded-frame mailbox. Replacing the pending item never disarms the
// existing timer, so a continuous stream cannot debounce rendering forever.
export function createLatestRenderSlot() {
  let item = null;
  let armed = false;
  return {
    offer(next) {
      const replaced = item;
      item = next;
      const arm = !armed;
      if (arm) armed = true;
      return { arm, replaced };
    },
    take() {
      const current = item;
      item = null;
      armed = false;
      return current;
    },
    clear() {
      const current = item;
      item = null;
      armed = false;
      return current;
    },
  };
}

function crc8(bytes) {
  let crc = 0;
  for (const byte of bytes) {
    crc ^= byte;
    for (let bit = 0; bit < 8; bit++) {
      crc = (crc & 0x80) ? ((crc << 1) ^ 0x07) & 0xff : (crc << 1) & 0xff;
    }
  }
  return crc;
}

// Pattern wire shape: A5 preamble, uint32 epoch-ms low word, uint16 frame
// sequence, CRC-8/ATM over the timestamp+sequence bytes. Bits are MSB-first.
export function decodeTimestampPatternBits(bits) {
  if (!Array.isArray(bits) || bits.length !== 64) return null;
  const bytes = [];
  for (let offset = 0; offset < 64; offset += 8) {
    let byte = 0;
    for (let bit = 0; bit < 8; bit++) byte = (byte << 1) | (bits[offset + bit] ? 1 : 0);
    bytes.push(byte);
  }
  if (bytes[0] !== 0xa5 || crc8(bytes.slice(1, 7)) !== bytes[7]) return null;
  const timestamp = (((bytes[1] << 24) >>> 0) | (bytes[2] << 16) | (bytes[3] << 8) | bytes[4]) >>> 0;
  const sequence = (bytes[5] << 8) | bytes[6];
  return { timestamp, sequence };
}
