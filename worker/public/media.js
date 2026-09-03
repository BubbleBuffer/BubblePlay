export const VERSION = 1;
export const STREAM_LAB_ASSET_BUILD = "wss-v3-host20-edge12-bench2-hevc-tiles-bpip4-v15";
export const CODEC = Object.freeze({ HEVC: 1, H264: 2 });
export const FRAME_HEADER_BYTES = 24;
export const MAX_FRAGMENT_BYTES = 48 * 1024;
export const MAX_MEDIA_PACKET_BYTES = FRAME_HEADER_BYTES + MAX_FRAGMENT_BYTES;
export const V2_VERSION = 2;
export const V2_HEADER_BYTES = 24;
export const V2_DATA_SHARDS = 3;
export const V2_SHARD_COUNT = 4;
export const V2_MAX_ACCESS_UNIT_BYTES = 8 * 1024 * 1024;
export const V2_MAX_PACKET_BYTES = V2_HEADER_BYTES + Math.ceil(V2_MAX_ACCESS_UNIT_BYTES / V2_DATA_SHARDS);
export const V2_MAX_REASSEMBLY_BYTES = 16 * 1024 * 1024;
export const V2_MAX_PENDING_FRAMES = 8;
export const V2_FRAME_TTL_MS = 120;
export const V3_VERSION = 3;
export const V3_HEADER_BYTES = 28;
export const V3_MAX_LANES = 16;
export const V3_MAX_PACKET_BYTES = V3_HEADER_BYTES + V2_MAX_ACCESS_UNIT_BYTES;
export const LAB_HOST_TRACE_BYTES = 20;
export const LAB_HOST_TRACE_MAGIC = 0x42504854;
export const LAB_EDGE_TRACE_BYTES = 12;
export const LAB_EDGE_TRACE_MAGIC = 0x42504554;
export const STREAM_LAB_COMPATIBILITY = Object.freeze({
  schemaVersion: 1,
  id: STREAM_LAB_ASSET_BUILD,
  receiverBuild: STREAM_LAB_ASSET_BUILD,
  mediaProtocol: 3,
  hostTrace: Object.freeze({ version: 1, bytes: LAB_HOST_TRACE_BYTES }),
  edgeTrace: Object.freeze({ version: 1, bytes: LAB_EDGE_TRACE_BYTES }),
  benchStatusProtocol: 2,
  privilegedIpc: Object.freeze({ version: 4 }),
  roiModes: Object.freeze(["off", "damage", "text-edge", "feedback", "feedback-headroom"]),
  hybridModes: Object.freeze([
    "off", "png-static-roi", "png-quiescent-detail", "hevc-quiescent-tiles",
  ]),
  hybridCacheModes: Object.freeze(["dda-gated", "suppress-reactivate"]),
  requiredTelemetry: Object.freeze([
    "reconstructionFeedback:v1",
    "reconstructionFeedbackHeadroom:v1",
    "hybridActivity:png-quiescent-detail",
    "hevcQuiescentTiles:v1",
    "hybridDeferredReadback:v1",
    "ddaDamageAudit:v1",
    "hybridCacheSuppression:v1",
    "hybridCachePolicy:v1",
    "wssLaneLifecycle:v1",
    "signalingRecycle:v1",
    "networkRouteProbe:v2",
    "wssAckAttempt:v1",
    "wssImmediatePresentation:v1",
    "wssTailAwareDeadline:v1",
  ]),
});
export const HYBRID_FRAME_MAGIC = 0x42504831;
export const HYBRID_FRAME_VERSION = 1;
const V3_COMPLETED_FRAME_WINDOW = 256;
const PRODUCTION_WSS_TOPOLOGY = Object.freeze({ laneCount: 16, dataShards: 15, parityShards: 1 });
const V3_FEC_XOR = 0;
const V3_FEC_PQ = 1;

export function createSignalingRecycleDiagnostic(state = {}) {
  const counter = value => Number.isSafeInteger(value) && value >= 0 ? value : null;
  const laneCount = Math.min(V3_MAX_LANES, counter(state.laneCount) ?? 0);
  const openLanes = Math.min(laneCount, counter(state.openLanes) ?? 0);
  const reason = typeof state.reason === "string" && /^[a-z0-9][a-z0-9-]{0,95}$/.test(state.reason)
    ? state.reason
    : "unknown";
  const recoveryState = typeof state.recoveryState === "string" &&
      /^[a-z0-9][a-z0-9-]{0,63}$/.test(state.recoveryState)
    ? state.recoveryState
    : null;
  const renderedAgeMs = Number.isFinite(state.renderedAgeMs) && state.renderedAgeMs >= 0
    ? Math.min(3_600_000, Math.round(state.renderedAgeMs))
    : null;
  return {
    protocol: 1,
    reason,
    signalingGeneration: counter(state.signalingGeneration),
    signalingRegistered: state.signalingRegistered === true,
    socketReadyState: Number.isInteger(state.socketReadyState) &&
        state.socketReadyState >= 0 && state.socketReadyState <= 3
      ? state.socketReadyState
      : null,
    usingWssMedia: state.usingWssMedia === true,
    wssProtocol: state.wssProtocol === 2 || state.wssProtocol === 3 ? state.wssProtocol : null,
    laneCount,
    openLanes,
    mediaGeneration: counter(state.mediaGeneration),
    reconstructed: counter(state.reconstructed),
    presented: counter(state.presented),
    receivedPackets: counter(state.receivedPackets),
    recoveryState,
    renderedAgeMs,
    online: state.online === true,
  };
}

export class WssLaneLifecycleTelemetry {
  constructor({ eventLimit = 16 } = {}) {
    this.eventLimit = Math.max(4, Math.min(32, Number(eventLimit) || 16));
    this.reset();
  }
  reset() {
    this.lanes = [];
    this.generations = [];
  }
  currentGeneration(lane) {
    return this.lanes[lane]?.socketGeneration ?? null;
  }
  construct(lane, atMs = Date.now()) {
    if (!Number.isInteger(lane) || lane < 0 || lane >= V3_MAX_LANES) return null;
    const socketGeneration = (this.generations[lane] || 0) + 1;
    this.generations[lane] = socketGeneration;
    const events = this.lanes[lane]?.events || [];
    this.lanes[lane] = {
      lane,
      socketGeneration,
      state: "connecting",
      constructedAtMs: finiteTime(atMs),
      transportOpenAtMs: null,
      helloOkAtMs: null,
      workerRegisteredAtMs: null,
      mediaForwardAtMs: null,
      mediaForwardRegisteredAtMs: null,
      firstPacketAtMs: null,
      firstPacketBytes: 0,
      closeAtMs: null,
      closeCode: null,
      closeReason: null,
      closeWasClean: null,
      errorAtMs: null,
      reconnectScheduledAtMs: null,
      reconnectDelayMs: null,
      reconnectReason: null,
      reconnectFiredAtMs: null,
      events,
    };
    this.record(lane, socketGeneration, "socket-construct", {}, atMs);
    return socketGeneration;
  }
  transportOpen(lane, generation, atMs = Date.now()) {
    const current = this.match(lane, generation);
    if (!current) return false;
    current.state = "transport-open";
    current.transportOpenAtMs = finiteTime(atMs);
    return this.record(lane, generation, "transport-open", {}, atMs);
  }
  helloOk(lane, generation, registeredAtMs, atMs = Date.now()) {
    const current = this.match(lane, generation);
    if (!current) return false;
    current.state = "registered";
    current.helloOkAtMs = finiteTime(atMs);
    current.workerRegisteredAtMs = finiteTime(registeredAtMs);
    return this.record(lane, generation, "hello-ok", {
      workerRegisteredAtMs: current.workerRegisteredAtMs,
    }, atMs);
  }
  mediaForward(lane, generation, registeredAtMs, atMs = Date.now()) {
    const current = this.match(lane, generation);
    if (!current) return false;
    current.mediaForwardAtMs = finiteTime(atMs);
    current.mediaForwardRegisteredAtMs = finiteTime(registeredAtMs);
    return this.record(lane, generation, "media-forward", {
      workerRegisteredAtMs: current.mediaForwardRegisteredAtMs,
      matchesHello: current.workerRegisteredAtMs !== null &&
        current.workerRegisteredAtMs === current.mediaForwardRegisteredAtMs,
    }, atMs);
  }
  firstPacket(lane, generation, bytes, atMs = Date.now()) {
    const current = this.match(lane, generation);
    if (!current || current.firstPacketAtMs !== null) return false;
    current.state = "receiving";
    current.firstPacketAtMs = finiteTime(atMs);
    current.firstPacketBytes = Math.max(0, Number(bytes) || 0);
    return this.record(lane, generation, "first-packet", {
      bytes: current.firstPacketBytes,
      workerRegisteredAtMs: current.workerRegisteredAtMs,
      mediaForwardRegisteredAtMs: current.mediaForwardRegisteredAtMs,
    }, atMs);
  }
  error(lane, generation, atMs = Date.now()) {
    const current = this.match(lane, generation);
    if (!current) return false;
    current.state = "error";
    current.errorAtMs = finiteTime(atMs);
    return this.record(lane, generation, "socket-error", {}, atMs);
  }
  close(lane, generation, event = {}, atMs = Date.now()) {
    const current = this.match(lane, generation);
    if (!current) return false;
    current.state = "closed";
    current.closeAtMs = finiteTime(atMs);
    current.closeCode = Number.isInteger(event.code) ? event.code : null;
    current.closeReason = String(event.reason || "").slice(0, 96);
    current.closeWasClean = typeof event.wasClean === "boolean" ? event.wasClean : null;
    return this.record(lane, generation, "socket-close", {
      code: current.closeCode,
      reason: current.closeReason,
      wasClean: current.closeWasClean,
    }, atMs);
  }
  reconnectScheduled(lane, generation, reason, delayMs, atMs = Date.now()) {
    const current = this.match(lane, generation);
    if (!current) return false;
    current.state = "reconnect-scheduled";
    current.reconnectScheduledAtMs = finiteTime(atMs);
    current.reconnectDelayMs = Math.max(0, Number(delayMs) || 0);
    current.reconnectReason = String(reason || "unknown").slice(0, 96);
    return this.record(lane, generation, "reconnect-scheduled", {
      reason: current.reconnectReason,
      delayMs: current.reconnectDelayMs,
    }, atMs);
  }
  reconnectFired(lane, generation, reason, atMs = Date.now()) {
    const current = this.match(lane, generation);
    if (!current) return false;
    current.state = "reconnecting";
    current.reconnectFiredAtMs = finiteTime(atMs);
    return this.record(lane, generation, "reconnect-fired", {
      reason: String(reason || current.reconnectReason || "unknown").slice(0, 96),
    }, atMs);
  }
  match(lane, generation) {
    const current = this.lanes[lane];
    return current?.socketGeneration === generation ? current : null;
  }
  record(lane, generation, type, detail, atMs) {
    const current = this.match(lane, generation);
    if (!current) return false;
    current.events.push({
      type,
      atMs: finiteTime(atMs),
      socketGeneration: generation,
      ...detail,
    });
    if (current.events.length > this.eventLimit)
      current.events.splice(0, current.events.length - this.eventLimit);
    return true;
  }
  snapshot() {
    return this.lanes.map(lane => lane ? {
      ...lane,
      events: lane.events.map(event => ({ ...event })),
    } : null);
  }
}

function finiteTime(value) {
  const time = Number(value);
  return Number.isFinite(time) && time >= 0 ? time : null;
}

function isSelfContainedHevcIdr(accessUnit) {
  if (!(accessUnit?.byteLength >= 6) ||
      !(accessUnit[0] === 0 && accessUnit[1] === 0 &&
        (accessUnit[2] === 1 || (accessUnit[2] === 0 && accessUnit[3] === 1)))) return false;
  let sawVps = false;
  let sawSps = false;
  let sawPps = false;
  let sawIdr = false;
  for (let offset = 0; offset + 3 <= accessUnit.byteLength;) {
    const startCodeBytes = accessUnit[offset] === 0 && accessUnit[offset + 1] === 0 &&
      accessUnit[offset + 2] === 0 && accessUnit[offset + 3] === 1 ? 4 :
      accessUnit[offset] === 0 && accessUnit[offset + 1] === 0 && accessUnit[offset + 2] === 1 ? 3 : 0;
    if (!startCodeBytes) {
      offset += 1;
      continue;
    }
    const nalOffset = offset + startCodeBytes;
    if (nalOffset + 2 > accessUnit.byteLength) return false;
    const nalType = (accessUnit[nalOffset] >> 1) & 0x3f;
    if (nalType === 19 || nalType === 20) sawIdr = true;
    else if (nalType === 32) sawVps = true;
    else if (nalType === 33) sawSps = true;
    else if (nalType === 34) sawPps = true;
    offset = nalOffset + 2;
  }
  return sawVps && sawSps && sawPps && sawIdr;
}

export function decodeHybridFrameV1(value) {
  const bytes = asBytes(value);
  if (!bytes || bytes.byteLength < 32) return null;
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  if (view.getUint32(0) !== HYBRID_FRAME_MAGIC || view.getUint8(4) !== HYBRID_FRAME_VERSION ||
      view.getUint16(6) !== 0 || view.getUint32(28) !== 0) return null;
  const operationCount = view.getUint8(5);
  if (operationCount > 13) return null;
  const generation = view.getUint32(8);
  const sequence = view.getUint32(12);
  const cropRevision = view.getUint32(16);
  const accessUnitLength = view.getUint32(20);
  const sidecarLength = view.getUint32(24);
  if (!accessUnitLength || sidecarLength > 256 * 1024 ||
      32 + sidecarLength + accessUnitLength !== bytes.byteLength) return null;
  const sidecarEnd = 32 + sidecarLength;
  const operations = [];
  let offset = 32;
  for (let index = 0; index < operationCount; index += 1) {
    if (offset + 4 > sidecarEnd || view.getUint8(offset + 1) !== 0) return null;
    const type = view.getUint8(offset);
    const tileId = view.getUint16(offset + 2);
    offset += 4;
    if (type === 3 && tileId === 0) operations.push({ type: "clear" });
    else if (type === 2) operations.push({ type: "invalidate", tileId });
    else if (type >= 4 && type <= 6) {
      if (offset + 8 > sidecarEnd) return null;
      const contentRevision = view.getUint32(offset);
      const validationSequence = view.getUint32(offset + 4);
      offset += 8;
      operations.push({
        type: type === 4 ? "suppress" : type === 5 ? "reactivate" : "invalidate-validated",
        tileId,
        contentRevision,
        validationSequence,
      });
    }
    else if (type === 1 || type === 7) {
      if (offset + 12 > sidecarEnd) return null;
      const x = view.getUint16(offset);
      const y = view.getUint16(offset + 2);
      const width = view.getUint16(offset + 4);
      const height = view.getUint16(offset + 6);
      const payloadLength = view.getUint32(offset + 8);
      offset += 12;
      if (!width || !height || width > 512 || height > 512 ||
          offset + payloadLength > sidecarEnd) return null;
      const payload = bytes.slice(offset, offset + payloadLength);
      if (type === 1) {
        if (payload.byteLength < 8 || payload[0] !== 137 || payload[1] !== 80 ||
            payload[2] !== 78 || payload[3] !== 71 || payload[4] !== 13 ||
            payload[5] !== 10 || payload[6] !== 26 || payload[7] !== 10) return null;
        operations.push({ type: "upsert", tileId, rect: { x, y, width, height }, png: payload });
      } else {
        if (width !== 128 || height !== 128 || !isSelfContainedHevcIdr(payload)) return null;
        operations.push({
          type: "hevc-upsert",
          tileId,
          rect: { x, y, width, height },
          accessUnit: payload,
        });
      }
      offset += payloadLength;
    } else return null;
  }
  if (offset !== sidecarEnd) return null;
  return {
    generation,
    sequence,
    cropRevision,
    operations,
    accessUnit: bytes.slice(sidecarEnd),
  };
}

export class HybridTileCache {
  constructor({ decodeImage = async png =>
    createImageBitmap(new Blob([png], { type: "image/png" })) } = {}) {
    this.decodeImage = decodeImage;
    this.tiles = new Map();
    this.generation = null;
    this.cropRevision = null;
    this.token = 0;
    this.transition = null;
    this.stats = {
      receivedUpserts: 0,
      receivedInvalidations: 0,
      receivedClears: 0,
      receivedSuppressions: 0,
      receivedReactivations: 0,
      receivedPermanentInvalidations: 0,
      staleOperationRejects: 0,
      pngBytes: 0,
      composites: 0,
      staleDrawRejects: 0,
      staleComposites: 0,
      geometryTransitions: 0,
      baseOnlyTransitionFrames: 0,
      maxBaseOnlyTransitionFrames: 0,
      tileReadyDelayFrames: 0,
      maxTileReadyDelayFrames: 0,
    };
  }
  ingest(frame) {
    if (!frame || !Number.isInteger(frame.generation) || !Number.isInteger(frame.sequence) ||
        !Number.isInteger(frame.cropRevision) || !Array.isArray(frame.operations)) return false;
    if (this.generation !== null && frame.generation < this.generation) return false;
    if (frame.generation !== this.generation || frame.cropRevision !== this.cropRevision) {
      this.clear();
      this.generation = frame.generation;
      this.cropRevision = frame.cropRevision;
      this.stats.geometryTransitions++;
      this.transition = {
        generation: frame.generation,
        cropRevision: frame.cropRevision,
        sequence: frame.sequence >>> 0,
        baseOnlyFrames: 0,
        lastBaseOnlySequence: null,
      };
    }
    for (const operation of frame.operations) {
      if (operation.type === "clear") {
        this.stats.receivedClears++;
        this.clear();
        this.generation = frame.generation;
        this.cropRevision = frame.cropRevision;
        this.transition = {
          generation: frame.generation,
          cropRevision: frame.cropRevision,
          sequence: frame.sequence >>> 0,
          baseOnlyFrames: 0,
          lastBaseOnlySequence: null,
        };
      } else if (operation.type === "invalidate") {
        this.stats.receivedInvalidations++;
        this.remove(operation.tileId);
      } else if (operation.type === "suppress") {
        this.stats.receivedSuppressions++;
        const tile = this.tiles.get(operation.tileId);
        if (!tile || tile.contentRevision !== (operation.contentRevision >>> 0)) {
          this.stats.staleOperationRejects++;
          continue;
        }
        tile.suppressed = true;
        tile.validationSequence = operation.validationSequence >>> 0;
      } else if (operation.type === "reactivate") {
        this.stats.receivedReactivations++;
        const tile = this.tiles.get(operation.tileId);
        if (!tile || !tile.suppressed ||
            tile.contentRevision !== (operation.contentRevision >>> 0) ||
            tile.validationSequence !== (operation.validationSequence >>> 0)) {
          this.stats.staleOperationRejects++;
          continue;
        }
        tile.suppressed = false;
        tile.validationSequence = null;
      } else if (operation.type === "invalidate-validated") {
        const tile = this.tiles.get(operation.tileId);
        if (!tile || !tile.suppressed ||
            tile.contentRevision !== (operation.contentRevision >>> 0) ||
            tile.validationSequence !== (operation.validationSequence >>> 0)) {
          this.stats.staleOperationRejects++;
          continue;
        }
        this.stats.receivedPermanentInvalidations++;
        this.remove(operation.tileId);
      } else if (operation.type === "upsert") {
        this.stats.receivedUpserts++;
        this.stats.pngBytes += operation.png?.byteLength || 0;
        this.remove(operation.tileId);
        const token = ++this.token;
        const pending = {
          token,
          generation: frame.generation,
          cropRevision: frame.cropRevision,
          contentRevision: frame.sequence >>> 0,
          readySequence: (frame.sequence + 1) >>> 0,
          rect: { ...operation.rect },
          bitmap: null,
          suppressed: false,
          validationSequence: null,
        };
        this.tiles.set(operation.tileId, pending);
        Promise.resolve(this.decodeImage(operation.png)).then(bitmap => {
          const current = this.tiles.get(operation.tileId);
          if (!bitmap || current?.token !== token) {
            try { bitmap?.close?.(); } catch {}
            return;
          }
          current.bitmap = bitmap;
        }).catch(() => {
          if (this.tiles.get(operation.tileId)?.token === token) this.tiles.delete(operation.tileId);
        });
      }
    }
    return true;
  }
  draw(ctx, metadata) {
    if (!ctx || metadata?.generation !== this.generation ||
        metadata?.cropRevision !== this.cropRevision || !Number.isInteger(metadata?.sequence)) {
      if (this.tiles.size) this.stats.staleDrawRejects++;
      return 0;
    }
    let drawn = 0;
    let readyDelay = 0;
    for (const tile of this.tiles.values()) {
      const age = (metadata.sequence - tile.readySequence) >>> 0;
      if (!tile.bitmap || tile.suppressed || age >= 0x8000_0000) continue;
      ctx.drawImage(tile.bitmap, tile.rect.x, tile.rect.y, tile.rect.width, tile.rect.height);
      drawn++;
      readyDelay = Math.max(readyDelay, age + 1);
    }
    this.stats.composites += drawn;
    if (drawn > 0) {
      this.stats.tileReadyDelayFrames += readyDelay;
      this.stats.maxTileReadyDelayFrames = Math.max(
        this.stats.maxTileReadyDelayFrames,
        readyDelay,
      );
      this.transition = null;
    } else if (this.transition?.generation === metadata.generation &&
        this.transition?.cropRevision === metadata.cropRevision &&
        this.transition.lastBaseOnlySequence !== (metadata.sequence >>> 0)) {
      this.transition.lastBaseOnlySequence = metadata.sequence >>> 0;
      this.transition.baseOnlyFrames++;
      this.stats.baseOnlyTransitionFrames++;
      this.stats.maxBaseOnlyTransitionFrames = Math.max(
        this.stats.maxBaseOnlyTransitionFrames,
        this.transition.baseOnlyFrames,
      );
    }
    return drawn;
  }
  remove(tileId) {
    const tile = this.tiles.get(tileId);
    if (!tile) return;
    try { tile.bitmap?.close?.(); } catch {}
    this.tiles.delete(tileId);
  }
  clear() {
    for (const tile of this.tiles.values()) try { tile.bitmap?.close?.(); } catch {}
    this.tiles.clear();
    this.token++;
  }
  snapshot() {
    let readyTiles = 0;
    let suppressedTiles = 0;
    let cachedBitmapBytes = 0;
    for (const tile of this.tiles.values()) {
      if (tile.bitmap) {
        readyTiles++;
        cachedBitmapBytes += tile.rect.width * tile.rect.height * 4;
      }
      if (tile.suppressed) suppressedTiles++;
    }
    return {
      generation: this.generation,
      cropRevision: this.cropRevision,
      activeTiles: this.tiles.size,
      drawableTiles: this.tiles.size - suppressedTiles,
      suppressedTiles,
      readyTiles,
      pendingTiles: this.tiles.size - readyTiles,
      cachedBitmapBytes,
      ...this.stats,
    };
  }
}

export const HEVC_TILE_CODEC = "hev1.1.6.L120.B0";

export function hevcTileDecoderConfig(codec = HEVC_TILE_CODEC) {
  return {
    codec,
    codedWidth: 128,
    codedHeight: 128,
    hardwareAcceleration: "prefer-hardware",
    optimizeForLatency: true,
  };
}

export async function probeHevcTileDecoderSupport(isConfigSupported, codec = HEVC_TILE_CODEC) {
  if (typeof isConfigSupported !== "function") {
    return { state: "api-unavailable", codec };
  }
  try {
    const support = await isConfigSupported(hevcTileDecoderConfig(codec));
    return { state: support?.supported === true ? "supported" : "unsupported", codec };
  } catch (error) {
    return {
      state: "error",
      codec,
      reason: String(error?.message || error || "capability probe failed").slice(0, 160),
    };
  }
}

export class HevcTileCache {
  constructor({
    createDecoder = callbacks => new VideoDecoder(callbacks),
    createChunk = init => new EncodedVideoChunk(init),
    codec = HEVC_TILE_CODEC,
    maxQueuedUpdates = 4,
  } = {}) {
    this.createDecoder = createDecoder;
    this.createChunk = createChunk;
    this.codec = codec;
    this.maxQueuedUpdates = Math.max(1, Math.min(4, Number(maxQueuedUpdates) || 4));
    this.decoder = null;
    this.tiles = new Map();
    this.pending = new Map();
    this.decodeQueue = [];
    this.inFlightTimestamp = null;
    this.generation = null;
    this.cropRevision = null;
    this.nextTimestamp = 1;
    this.decoderCapability = { state: "unprobed", codec };
    this.stats = {
      receivedUpserts: 0,
      receivedInvalidations: 0,
      receivedClears: 0,
      receivedSuppressions: 0,
      receivedReactivations: 0,
      receivedPermanentInvalidations: 0,
      staleOperationRejects: 0,
      hevcBytes: 0,
      decodedTiles: 0,
      decoderErrors: 0,
      decoderPressureBypasses: 0,
      decoderCapabilityBypasses: 0,
      staleOutputRejects: 0,
      staleDrawRejects: 0,
      composites: 0,
    };
  }

  setDecoderCapability(capability) {
    const state = capability?.state;
    if (!["supported", "unsupported", "api-unavailable", "error"].includes(state)) return false;
    this.decoderCapability = {
      state,
      codec: String(capability.codec || this.codec),
      ...(capability.reason ? { reason: String(capability.reason).slice(0, 160) } : {}),
    };
    if (state !== "supported") {
      this.resetDecoderWork();
      this.clearTiles();
      try { this.decoder?.close?.(); } catch {}
      this.decoder = null;
    }
    return true;
  }

  ensureDecoder() {
    if (!this.decoder || this.decoder.state === "closed") {
      this.decoder = this.createDecoder({
        output: frame => this.acceptDecodedFrame(frame),
        error: () => this.handleDecoderError(),
      });
    }
    if (this.decoder.state !== "configured") {
      this.decoder.configure({
        codec: this.codec,
        codedWidth: 128,
        codedHeight: 128,
        hardwareAcceleration: "prefer-hardware",
        optimizeForLatency: true,
      });
    }
  }

  ingest(frame) {
    if (!frame || !Number.isInteger(frame.generation) || !Number.isInteger(frame.sequence) ||
        !Number.isInteger(frame.cropRevision) || !Array.isArray(frame.operations)) return false;
    if (this.generation !== null && frame.generation < this.generation) return false;
    if (frame.generation !== this.generation || frame.cropRevision !== this.cropRevision) {
      this.resetDecoderWork();
      this.clearTiles();
      this.generation = frame.generation;
      this.cropRevision = frame.cropRevision;
    }
    for (const operation of frame.operations) {
      if (operation.type === "clear") {
        this.stats.receivedClears++;
        this.resetDecoderWork();
        this.clearTiles();
      } else if (operation.type === "invalidate") {
        this.stats.receivedInvalidations++;
        this.remove(operation.tileId);
      } else if (operation.type === "suppress") {
        this.stats.receivedSuppressions++;
        const tile = this.tiles.get(operation.tileId);
        if (!tile || tile.contentRevision !== (operation.contentRevision >>> 0)) {
          this.stats.staleOperationRejects++;
          continue;
        }
        tile.suppressed = true;
        tile.validationSequence = operation.validationSequence >>> 0;
      } else if (operation.type === "reactivate") {
        this.stats.receivedReactivations++;
        const tile = this.tiles.get(operation.tileId);
        if (!tile || !tile.suppressed ||
            tile.contentRevision !== (operation.contentRevision >>> 0) ||
            tile.validationSequence !== (operation.validationSequence >>> 0)) {
          this.stats.staleOperationRejects++;
          continue;
        }
        tile.suppressed = false;
        tile.validationSequence = null;
      } else if (operation.type === "invalidate-validated") {
        const tile = this.tiles.get(operation.tileId);
        if (!tile || !tile.suppressed ||
            tile.contentRevision !== (operation.contentRevision >>> 0) ||
            tile.validationSequence !== (operation.validationSequence >>> 0)) {
          this.stats.staleOperationRejects++;
          continue;
        }
        this.stats.receivedPermanentInvalidations++;
        this.remove(operation.tileId);
      } else if (operation.type === "hevc-upsert") {
        this.stats.receivedUpserts++;
        this.stats.hevcBytes += operation.accessUnit?.byteLength || 0;
        if (["unsupported", "api-unavailable", "error"].includes(
          this.decoderCapability.state,
        )) {
          this.stats.decoderCapabilityBypasses++;
          continue;
        }
        if (operation.rect?.width !== 128 || operation.rect?.height !== 128 ||
            !operation.accessUnit?.byteLength) {
          this.stats.decoderErrors++;
          continue;
        }
        const queued = this.decodeQueue.length + Number(this.inFlightTimestamp !== null);
        if (queued >= this.maxQueuedUpdates) {
          this.stats.decoderPressureBypasses++;
          continue;
        }
        this.remove(operation.tileId);
        const timestamp = this.nextTimestamp++;
        const pending = {
          timestamp,
          tileId: operation.tileId,
          generation: frame.generation,
          cropRevision: frame.cropRevision,
          contentRevision: frame.sequence >>> 0,
          readySequence: (frame.sequence + 1) >>> 0,
          rect: { ...operation.rect },
          accessUnit: operation.accessUnit,
          frame: null,
          suppressed: false,
          validationSequence: null,
        };
        this.tiles.set(operation.tileId, pending);
        this.pending.set(timestamp, pending);
        this.decodeQueue.push(pending);
      }
    }
    this.pumpDecoder();
    return true;
  }

  pumpDecoder() {
    if (this.inFlightTimestamp !== null || this.decodeQueue.length === 0) return;
    const pending = this.decodeQueue.shift();
    if (this.pending.get(pending.timestamp) !== pending) {
      this.pumpDecoder();
      return;
    }
    try {
      this.ensureDecoder();
      this.inFlightTimestamp = pending.timestamp;
      this.decoder.decode(this.createChunk({
        type: "key",
        timestamp: pending.timestamp,
        data: pending.accessUnit,
      }));
    } catch {
      this.handleDecoderError();
    }
  }

  acceptDecodedFrame(frame) {
    const timestamp = Number(frame?.timestamp);
    const pending = this.pending.get(timestamp);
    this.inFlightTimestamp = null;
    if (!pending || pending.generation !== this.generation ||
        pending.cropRevision !== this.cropRevision ||
        this.tiles.get(pending.tileId) !== pending ||
        (Number.isFinite(frame?.codedWidth) && frame.codedWidth !== 128) ||
        (Number.isFinite(frame?.codedHeight) && frame.codedHeight !== 128)) {
      this.stats.staleOutputRejects++;
      try { frame?.close?.(); } catch {}
    } else {
      this.pending.delete(timestamp);
      pending.accessUnit = null;
      pending.frame = frame;
      this.stats.decodedTiles++;
    }
    this.pumpDecoder();
  }

  draw(ctx, metadata) {
    if (!ctx || metadata?.generation !== this.generation ||
        metadata?.cropRevision !== this.cropRevision || !Number.isInteger(metadata?.sequence)) {
      if (this.tiles.size) this.stats.staleDrawRejects++;
      return 0;
    }
    let drawn = 0;
    for (const tile of this.tiles.values()) {
      const age = (metadata.sequence - tile.readySequence) >>> 0;
      if (!tile.frame || tile.suppressed || age >= 0x8000_0000) continue;
      ctx.drawImage(tile.frame, tile.rect.x, tile.rect.y, tile.rect.width, tile.rect.height);
      drawn++;
    }
    this.stats.composites += drawn;
    return drawn;
  }

  remove(tileId) {
    const tile = this.tiles.get(tileId);
    if (!tile) return;
    this.pending.delete(tile.timestamp);
    this.decodeQueue = this.decodeQueue.filter(entry => entry !== tile);
    try { tile.frame?.close?.(); } catch {}
    this.tiles.delete(tileId);
  }

  clearTiles() {
    for (const tile of this.tiles.values()) try { tile.frame?.close?.(); } catch {}
    this.tiles.clear();
    this.pending.clear();
    this.decodeQueue.length = 0;
  }

  clear() {
    this.resetDecoderWork();
    this.clearTiles();
  }

  resetDecoderWork() {
    this.inFlightTimestamp = null;
    this.pending.clear();
    this.decodeQueue.length = 0;
    if (this.decoder && this.decoder.state !== "closed") {
      try { this.decoder.reset(); } catch {}
    }
  }

  handleDecoderError() {
    this.stats.decoderErrors++;
    this.resetDecoderWork();
    this.clearTiles();
    try { this.decoder?.close?.(); } catch {}
    this.decoder = null;
  }

  snapshot() {
    let readyTiles = 0;
    let cachedFrameBytes = 0;
    for (const tile of this.tiles.values()) {
      if (tile.frame) {
        readyTiles++;
        cachedFrameBytes += 128 * 128 * 4;
      }
    }
    return {
      generation: this.generation,
      cropRevision: this.cropRevision,
      activeTiles: this.tiles.size,
      readyTiles,
      pendingTiles: this.tiles.size - readyTiles,
      queuedUpdates: this.decodeQueue.length + Number(this.inFlightTimestamp !== null),
      cachedFrameBytes,
      decoderCapability: this.decoderCapability.state,
      decoderCapabilityCodec: this.decoderCapability.codec,
      ...(this.decoderCapability.reason
        ? { decoderCapabilityReason: this.decoderCapability.reason }
        : {}),
      ...this.stats,
    };
  }

  close() {
    this.resetDecoderWork();
    this.clearTiles();
    try { this.decoder?.close?.(); } catch {}
    this.decoder = null;
  }
}

export function drawBaseThenHybrid(ctx, frame, cache, metadata, width, height) {
  ctx.drawImage(frame, 0, 0, width, height);
  return cache.draw(ctx, metadata);
}

export class FramePresentationTelemetry {
  constructor() { this.reset(); }

  reset() {
    this.immediateAttempts = 0;
    this.immediatePresented = 0;
    this.rafEnqueued = 0;
    this.rafCoalesced = 0;
    this.rafLateDrops = 0;
  }

  noteImmediateAttempt() { this.immediateAttempts++; }
  noteImmediatePresented() { this.immediatePresented++; }
  noteRafEnqueued() { this.rafEnqueued++; }
  noteRafCoalesced(count = 1) {
    const bounded = Number.isSafeInteger(count) && count > 0 ? count : 1;
    this.rafCoalesced += bounded;
  }
  noteRafLateDrop() { this.rafLateDrops++; }

  snapshot() {
    return {
      protocol: 1,
      immediateAttempts: this.immediateAttempts,
      immediatePresented: this.immediatePresented,
      rafEnqueued: this.rafEnqueued,
      rafCoalesced: this.rafCoalesced,
      rafLateDrops: this.rafLateDrops,
    };
  }
}

export function presentWssFrameImmediately(frame, usingWssMedia, due, draw, telemetry = null) {
  if (!usingWssMedia) return false;
  telemetry?.noteImmediateAttempt?.();
  draw(frame, due);
  return true;
}

export function prepareHybridWssFrame(frame, { active = false, cache = null } = {}) {
  const hybrid = decodeHybridFrameV1(frame?.data);
  if (!active && !hybrid) return {
    data: frame?.data,
    cropRevision: null,
    activity: null,
  };
  if (!hybrid || hybrid.generation !== frame?.generation || hybrid.sequence !== frame?.sequence ||
      !cache?.ingest(hybrid)) return null;
  return {
    data: hybrid.accessUnit,
    cropRevision: hybrid.cropRevision,
    activity: {
      upserts: hybrid.operations.filter(operation => operation.type === "upsert").length,
      hevcUpserts: hybrid.operations.filter(operation => operation.type === "hevc-upsert").length,
      invalidations: hybrid.operations.filter(operation => operation.type === "invalidate").length,
      suppressions: hybrid.operations.filter(operation => operation.type === "suppress").length,
      reactivations: hybrid.operations.filter(operation => operation.type === "reactivate").length,
      permanentInvalidations: hybrid.operations.filter(
        operation => operation.type === "invalidate-validated").length,
      clears: hybrid.operations.filter(operation => operation.type === "clear").length,
      pngBytes: hybrid.operations.reduce((sum, operation) =>
        sum + (operation.type === "upsert" ? operation.png.byteLength : 0), 0),
      hevcBytes: hybrid.operations.reduce((sum, operation) =>
        sum + (operation.type === "hevc-upsert" ? operation.accessUnit.byteLength : 0), 0),
    },
  };
}

export function stripLabEdgeTrace(packet, options = {}) {
  const requireEdgeTrace = options.requireEdgeTrace === true;
  const requireHostTrace = options.requireHostTrace === true;
  const bytes = packet instanceof ArrayBuffer
    ? new Uint8Array(packet)
    : ArrayBuffer.isView(packet)
      ? new Uint8Array(packet.buffer, packet.byteOffset, packet.byteLength)
      : null;
  if (!bytes || bytes.byteLength < LAB_EDGE_TRACE_BYTES) {
    return requireEdgeTrace
      ? { packet, edgeReceivedAtEpochMs: null, rejection: "edge-trailer-format" }
      : { packet, edgeReceivedAtEpochMs: null };
  }
  const trailerOffset = bytes.byteLength - LAB_EDGE_TRACE_BYTES;
  const trailer = new DataView(bytes.buffer, bytes.byteOffset + trailerOffset, LAB_EDGE_TRACE_BYTES);
  if (trailer.getUint32(0) !== LAB_EDGE_TRACE_MAGIC) {
    return requireEdgeTrace
      ? { packet, edgeReceivedAtEpochMs: null, rejection: "edge-trailer-format" }
      : { packet, edgeReceivedAtEpochMs: null };
  }
  const edgeReceivedAtEpochMs = trailer.getFloat64(4);
  if (!Number.isFinite(edgeReceivedAtEpochMs)) {
    return requireEdgeTrace
      ? { packet, edgeReceivedAtEpochMs: null, rejection: "edge-trailer-format" }
      : { packet, edgeReceivedAtEpochMs: null };
  }
  let packetEnd = trailerOffset;
  let hostSentAtEpochMs = null;
  let hostRoiTrace = null;
  if (packetEnd >= LAB_HOST_TRACE_BYTES) {
    const hostOffset = packetEnd - LAB_HOST_TRACE_BYTES;
    const hostTrailer = new DataView(bytes.buffer, bytes.byteOffset + hostOffset, LAB_HOST_TRACE_BYTES);
    if (hostTrailer.getUint32(0) === LAB_HOST_TRACE_MAGIC && hostTrailer.getUint8(12) !== 1) {
      return {
        packet,
        edgeReceivedAtEpochMs,
        hostSentAtEpochMs: null,
        hostRoiTrace: null,
        rejection: "host-trailer-version",
      };
    }
    if (hostTrailer.getUint32(0) === LAB_HOST_TRACE_MAGIC && hostTrailer.getUint8(12) === 1) {
      const candidate = hostTrailer.getFloat64(4);
      const roiReasons = ["none", "applied", "missing-damage", "empty-damage", "idr",
        "coverage-at-least-half", "unsupported-encoder-state"];
      const reason = roiReasons[hostTrailer.getUint8(13)] ?? "unknown";
      if (Number.isFinite(candidate)) {
        hostSentAtEpochMs = candidate;
        hostRoiTrace = {
          reason,
          changedCtbs: hostTrailer.getUint16(14),
          totalCtbs: hostTrailer.getUint16(16),
        };
        packetEnd = hostOffset;
      }
    }
  }
  if (hostSentAtEpochMs === null && packetEnd >= 12) {
    const hostOffset = packetEnd - 12;
    const hostTrailer = new DataView(bytes.buffer, bytes.byteOffset + hostOffset, 12);
    if (hostTrailer.getUint32(0) === LAB_HOST_TRACE_MAGIC) {
      const candidate = hostTrailer.getFloat64(4);
      if (Number.isFinite(candidate)) {
        hostSentAtEpochMs = candidate;
        packetEnd = hostOffset;
      }
    }
  }
  if (requireHostTrace && hostSentAtEpochMs === null) {
    return {
      packet,
      edgeReceivedAtEpochMs,
      hostSentAtEpochMs: null,
      hostRoiTrace: null,
      rejection: "host-trailer-format",
    };
  }
  return { packet: bytes.slice(0, packetEnd).buffer, edgeReceivedAtEpochMs, hostSentAtEpochMs, hostRoiTrace };
}

const GF_EXP = new Uint8Array(512);
const GF_LOG = new Uint8Array(256);
{
  let value = 1;
  for (let index = 0; index < 255; index += 1) {
    GF_EXP[index] = value;
    GF_LOG[value] = index;
    value <<= 1;
    if (value & 0x100) value ^= 0x11d;
  }
  for (let index = 255; index < GF_EXP.length; index += 1) GF_EXP[index] = GF_EXP[index - 255];
}

function gfMultiply(left, right) {
  return left && right ? GF_EXP[GF_LOG[left] + GF_LOG[right]] : 0;
}

function gfDivide(value, divisor) {
  if (!divisor) return null;
  return value ? GF_EXP[GF_LOG[value] + 255 - GF_LOG[divisor]] : 0;
}

function pqCoefficient(dataShard) {
  return GF_EXP[dataShard];
}

const DEADLINE_POLICY = Object.freeze({
  work: { start: 300, min: 180, max: 400, percentile: .95, margin: 40, decayEveryMs: 5_000 },
  game: { start: 120, min: 80, max: 150, percentile: .90, margin: 20, decayEveryMs: 5_000 },
});

export class AdaptiveFrameDeadline {
  constructor({ mode = "work", now = () => performance.now() } = {}) {
    this.now = now;
    this.samples = [];
    this.setMode(mode);
  }
  setMode(mode) {
    this.mode = mode === "game" ? "game" : "work";
    const policy = DEADLINE_POLICY[this.mode];
    this.deadlineMs = policy.start;
    this.p95Ms = 0;
    this.samples.length = 0;
    this.lastDamageAt = this.now();
    this.lastDecayAt = this.now();
  }
  observeCompletion(ageMs) {
    if (!Number.isFinite(ageMs) || ageMs < 0) return;
    this.samples.push(Math.min(2_000, ageMs));
    if (this.samples.length > 64) this.samples.shift();
    const sorted = [...this.samples].sort((a, b) => a - b);
    const policy = DEADLINE_POLICY[this.mode];
    const index = Math.max(0, Math.ceil(sorted.length * policy.percentile) - 1);
    this.p95Ms = Math.round(sorted[index] || 0);
    const target = Math.max(policy.min, Math.min(policy.max, this.p95Ms + policy.margin));
    if (target > this.deadlineMs) this.deadlineMs = target;
  }
  observeTail(ageMs) {
    if (!Number.isFinite(ageMs) || ageMs < 0) return;
    const policy = DEADLINE_POLICY[this.mode];
    const target = Math.max(
      policy.min,
      Math.min(policy.max, Math.ceil(Math.min(2_000, ageMs)) + policy.margin),
    );
    this.lastDamageAt = this.now();
    if (target > this.deadlineMs) this.deadlineMs = target;
  }
  noteExpiry() {
    const policy = DEADLINE_POLICY[this.mode];
    this.lastDamageAt = this.now();
    this.deadlineMs = Math.min(policy.max, this.deadlineMs + 40);
  }
  tick() {
    const now = this.now();
    const policy = DEADLINE_POLICY[this.mode];
    if (now - this.lastDamageAt < policy.decayEveryMs || now - this.lastDecayAt < policy.decayEveryMs) return;
    const target = Math.max(policy.min, Math.min(policy.max, this.p95Ms + policy.margin));
    this.deadlineMs = Math.max(target, this.deadlineMs - 10);
    this.lastDecayAt = now;
  }
}

export function applyLateShardDeadline({ reassembler, ordered, deadline } = {}) {
  if (!reassembler || !ordered || !deadline ||
      typeof reassembler.takeLateShardAges !== "function" ||
      typeof reassembler.setTtlMs !== "function" ||
      typeof ordered.setTtlMs !== "function" ||
      typeof deadline.observeTail !== "function") return null;
  const ages = reassembler.takeLateShardAges();
  if (!Array.isArray(ages)) return null;
  const validAges = ages.filter(ageMs => Number.isFinite(ageMs) && ageMs >= 0);
  if (!validAges.length) return null;
  const previousDeadlineMs = deadline.deadlineMs;
  for (const ageMs of validAges) deadline.observeTail(ageMs);
  const deadlineMs = deadline.deadlineMs;
  // The packet that supplied this evidence normally belongs to an already
  // parity-completed frame, so its browser handler has no reconstructed frame
  // to carry it through the ordinary completion path. Apply the raised expiry
  // boundary synchronously before that handler can return.
  reassembler.setTtlMs(deadlineMs);
  ordered.setTtlMs(deadlineMs);
  return {
    observations: validAges.length,
    maxAgeMs: Math.max(...validAges),
    previousDeadlineMs,
    deadlineMs,
    raisedByMs: Math.max(0, deadlineMs - previousDeadlineMs),
  };
}

function asBytes(value) {
  if (value instanceof Uint8Array) return value;
  if (value instanceof ArrayBuffer) return new Uint8Array(value);
  if (ArrayBuffer.isView(value)) return new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
  return null;
}

export function fragmentFrame({ codec, generation, gop, sequence, timestamp90k, key, data }) {
  const bytes = asBytes(data);
  if (!bytes || !Object.values(CODEC).includes(codec) || !Number.isInteger(generation) || !Number.isInteger(gop) || !Number.isInteger(sequence) || !Number.isInteger(timestamp90k)) return [];
  const count = Math.max(1, Math.ceil(bytes.byteLength / MAX_FRAGMENT_BYTES));
  if (count > 0xffff) return [];
  return Array.from({ length: count }, (_, index) => {
    const start = index * MAX_FRAGMENT_BYTES;
    const payload = bytes.subarray(start, Math.min(bytes.byteLength, start + MAX_FRAGMENT_BYTES));
    const packet = new Uint8Array(FRAME_HEADER_BYTES + payload.byteLength);
    const view = new DataView(packet.buffer);
    view.setUint8(0, VERSION); view.setUint8(1, codec); view.setUint8(2, key ? 1 : 0); view.setUint8(3, 0);
    view.setUint32(4, generation >>> 0); view.setUint32(8, gop >>> 0); view.setUint32(12, sequence >>> 0); view.setUint32(16, timestamp90k >>> 0);
    view.setUint16(20, index); view.setUint16(22, count);
    packet.set(payload, FRAME_HEADER_BYTES);
    return packet.buffer;
  });
}

export function decodeMediaPacket(packet) {
  const bytes = asBytes(packet);
  if (!bytes || bytes.byteLength < FRAME_HEADER_BYTES || bytes.byteLength > MAX_MEDIA_PACKET_BYTES) return null;
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const header = {
    version: view.getUint8(0), codec: view.getUint8(1), key: Boolean(view.getUint8(2) & 1),
    generation: view.getUint32(4), gop: view.getUint32(8), sequence: view.getUint32(12), timestamp90k: view.getUint32(16),
    fragmentIndex: view.getUint16(20), fragmentCount: view.getUint16(22),
  };
  if (header.version !== VERSION || !Object.values(CODEC).includes(header.codec) || !header.fragmentCount || header.fragmentIndex >= header.fragmentCount) return null;
  return { header, payload: bytes.slice(FRAME_HEADER_BYTES) };
}

export class FrameReassembler {
  constructor() { this.generation = null; this.gop = null; this.waitingForKey = true; this.partial = new Map(); this.partialByStripe = new Map(); }
  push(packet, stripe = 0) {
    const decoded = decodeMediaPacket(packet);
    if (!decoded) return null;
    const { header, payload } = decoded;
    if (this.generation !== null && header.generation < this.generation) return null;
    if (this.generation !== header.generation) {
      this.generation = header.generation; this.gop = null; this.waitingForKey = true; this.partial.clear(); this.partialByStripe.clear();
    }
    if (this.gop !== null && header.gop < this.gop) return null;
    if (this.waitingForKey && !header.key) return null;
    const id = `${header.generation}:${header.gop}:${header.sequence}`;
    let entry = this.partial.get(id);
    if (!entry) {
      const old = this.partialByStripe.get(stripe);
      if (old && old !== id) this.partial.delete(old);
      entry = { header, fragments: new Array(header.fragmentCount), received: 0, stripe };
      this.partial.set(id, entry); this.partialByStripe.set(stripe, id);
    }
    if (entry.header.fragmentCount !== header.fragmentCount || entry.fragments[header.fragmentIndex]) return null;
    entry.fragments[header.fragmentIndex] = payload; entry.received++;
    if (entry.received !== header.fragmentCount) return null;
    this.partial.delete(id);
    if (this.partialByStripe.get(entry.stripe) === id) this.partialByStripe.delete(entry.stripe);
    if (header.key) { this.waitingForKey = false; this.gop = header.gop; }
    else if (this.gop === null || header.gop !== this.gop) return null;
    const length = entry.fragments.reduce((sum, fragment) => sum + fragment.byteLength, 0);
    const data = new Uint8Array(length); let offset = 0;
    for (const fragment of entry.fragments) { data.set(fragment, offset); offset += fragment.byteLength; }
    return { ...header, data };
  }
  reset() { this.generation = this.gop = null; this.waitingForKey = true; this.partial.clear(); this.partialByStripe.clear(); }
}

export function shardFrameV2({ codec, generation, gop, sequence, timestamp90k, key, data }) {
  const bytes = asBytes(data);
  if (!bytes || bytes.byteLength === 0 || bytes.byteLength > V2_MAX_ACCESS_UNIT_BYTES ||
      !Object.values(CODEC).includes(codec)) return [];
  const fields = [generation, gop, sequence, timestamp90k];
  if (!fields.every(Number.isInteger)) return [];
  const shardLength = Math.ceil(bytes.byteLength / V2_DATA_SHARDS);
  const shards = Array.from({ length: V2_SHARD_COUNT }, () => new Uint8Array(shardLength));
  for (let shard = 0; shard < V2_DATA_SHARDS; shard += 1) {
    shards[shard].set(bytes.subarray(shard * shardLength, Math.min(bytes.byteLength, (shard + 1) * shardLength)));
  }
  for (let index = 0; index < shardLength; index += 1) {
    shards[3][index] = shards[0][index] ^ shards[1][index] ^ shards[2][index];
  }
  return shards.map((payload, shardIndex) => {
    const packet = new Uint8Array(V2_HEADER_BYTES + payload.byteLength);
    const view = new DataView(packet.buffer);
    view.setUint8(0, V2_VERSION); view.setUint8(1, codec); view.setUint8(2, key ? 1 : 0); view.setUint8(3, shardIndex);
    view.setUint32(4, generation >>> 0); view.setUint32(8, gop >>> 0); view.setUint32(12, sequence >>> 0);
    view.setUint32(16, timestamp90k >>> 0); view.setUint32(20, bytes.byteLength >>> 0);
    packet.set(payload, V2_HEADER_BYTES);
    return packet.buffer;
  });
}

export function validateWssTopology(topology) {
  if (!topology || typeof topology !== "object" || Array.isArray(topology)) return false;
  if (Object.keys(topology).some(key => !["laneCount", "dataShards", "parityShards"].includes(key))) return false;
  const { laneCount, dataShards, parityShards } = topology;
  return Number.isInteger(laneCount) && laneCount >= 1 && laneCount <= V3_MAX_LANES &&
    Number.isInteger(dataShards) && dataShards >= 1 && dataShards <= V3_MAX_LANES &&
    Number.isInteger(parityShards) && parityShards >= 0 && parityShards <= 2 &&
    laneCount === dataShards + parityShards;
}

export function shouldRecoverWssStripeReconnect({ protocol, topology } = {}) {
  return protocol !== V3_VERSION || !validateWssTopology(topology) || topology.parityShards === 0;
}

export function selectStaleWssLane({
  highestReconstructed,
  highestSequenceByLane,
  lastProgressAtMsByLane,
  openByLane,
  nowMs,
  maxLagFrames = 8,
  stallMs = 1_200,
} = {}) {
  if (!Number.isInteger(highestReconstructed) || highestReconstructed < 0 ||
      !Array.isArray(highestSequenceByLane) || !Array.isArray(lastProgressAtMsByLane) ||
      !Array.isArray(openByLane) || highestSequenceByLane.length !== lastProgressAtMsByLane.length ||
      highestSequenceByLane.length !== openByLane.length || !Number.isFinite(nowMs) ||
      !Number.isInteger(maxLagFrames) || maxLagFrames < 0 || !Number.isFinite(stallMs) || stallMs < 0) return null;
  let selected = null;
  let selectedLag = maxLagFrames;
  for (let lane = 0; lane < highestSequenceByLane.length; lane += 1) {
    if (openByLane[lane] !== true) continue;
    const sequence = highestSequenceByLane[lane];
    const progressedAt = lastProgressAtMsByLane[lane];
    if (!Number.isInteger(sequence) || sequence < 0 || !Number.isFinite(progressedAt) ||
        nowMs - progressedAt < stallMs) continue;
    const lag = (highestReconstructed - sequence) >>> 0;
    if (lag <= maxLagFrames || lag >= 0x8000_0000 || lag <= selectedLag) continue;
    selected = lane;
    selectedLag = lag;
  }
  return selected;
}

export function shouldDeferWssMediaStart({
  requestedTopology = null,
  capabilitiesGeneration = 0,
  signalingGeneration = 0,
} = {}) {
  return !validateWssTopology(requestedTopology) && capabilitiesGeneration !== signalingGeneration;
}

export function selectWssMediaTopology({
  requestedTopology = null,
  hostCapabilities = null,
  capabilitiesCurrent = true,
  v3Rejected = false,
} = {}) {
  if (v3Rejected) return null;
  if (validateWssTopology(requestedTopology)) return requestedTopology;
  if (!capabilitiesCurrent) return null;
  const protocols = Array.isArray(hostCapabilities?.mediaProtocols) ? hostCapabilities.mediaProtocols : [];
  const fec = Array.isArray(hostCapabilities?.wssFec) ? hostCapabilities.wssFec : [];
  return protocols.includes(3) &&
      hostCapabilities?.maxWssLanes >= PRODUCTION_WSS_TOPOLOGY.laneCount &&
      fec.includes("xor-1")
    ? PRODUCTION_WSS_TOPOLOGY
    : null;
}

export function shardFrameV3({ codec, generation, gop, sequence, timestamp90k, key, topology, data }) {
  const bytes = asBytes(data);
  if (!bytes || bytes.byteLength === 0 || bytes.byteLength > V2_MAX_ACCESS_UNIT_BYTES ||
      !Object.values(CODEC).includes(codec) || !validateWssTopology(topology)) return [];
  if (![generation, gop, sequence, timestamp90k].every(Number.isInteger)) return [];
  const shardLength = Math.ceil(bytes.byteLength / topology.dataShards);
  const shards = Array.from({ length: topology.laneCount }, () => new Uint8Array(shardLength));
  for (let shard = 0; shard < topology.dataShards; shard += 1) {
    shards[shard].set(bytes.subarray(shard * shardLength,
      Math.min(bytes.byteLength, (shard + 1) * shardLength)));
  }
  if (topology.parityShards > 0) {
    const p = shards[topology.dataShards];
    const q = topology.parityShards === 2 ? shards[topology.dataShards + 1] : null;
    for (let byte = 0; byte < shardLength; byte += 1) {
      for (let shard = 0; shard < topology.dataShards; shard += 1) {
        const value = shards[shard][byte];
        p[byte] ^= value;
        if (q) q[byte] ^= gfMultiply(pqCoefficient(shard), value);
      }
    }
  }
  return shards.map((payload, shardIndex) => {
    const packet = new Uint8Array(V3_HEADER_BYTES + payload.byteLength);
    const view = new DataView(packet.buffer);
    view.setUint8(0, V3_VERSION); view.setUint8(1, codec); view.setUint8(2, key ? 1 : 0); view.setUint8(3, shardIndex);
    view.setUint8(4, topology.laneCount); view.setUint8(5, topology.dataShards);
    view.setUint8(6, topology.parityShards);
    view.setUint8(7, topology.parityShards === 2 ? V3_FEC_PQ : V3_FEC_XOR);
    view.setUint32(8, generation >>> 0); view.setUint32(12, gop >>> 0); view.setUint32(16, sequence >>> 0);
    view.setUint32(20, timestamp90k >>> 0); view.setUint32(24, bytes.byteLength >>> 0);
    packet.set(payload, V3_HEADER_BYTES);
    return packet.buffer;
  });
}

function inspectMediaShardV3(packet) {
  const bytes = asBytes(packet);
  if (!bytes || bytes.byteLength < V3_HEADER_BYTES || bytes.byteLength > V3_MAX_PACKET_BYTES)
    return { decoded: null, rejection: "v3-length" };
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const header = {
    version: view.getUint8(0), codec: view.getUint8(1), key: Boolean(view.getUint8(2) & 1),
    shardIndex: view.getUint8(3), laneCount: view.getUint8(4), dataShards: view.getUint8(5),
    parityShards: view.getUint8(6), fecMode: view.getUint8(7),
    generation: view.getUint32(8), gop: view.getUint32(12),
    sequence: view.getUint32(16), timestamp90k: view.getUint32(20), originalLength: view.getUint32(24),
  };
  const topology = { laneCount: header.laneCount, dataShards: header.dataShards, parityShards: header.parityShards };
  const expected = header.dataShards ? Math.ceil(header.originalLength / header.dataShards) : 0;
  const expectedFecMode = header.parityShards === 2 ? V3_FEC_PQ : V3_FEC_XOR;
  if (header.version !== V3_VERSION || !Object.values(CODEC).includes(header.codec) ||
      header.fecMode !== expectedFecMode || !header.originalLength ||
      header.originalLength > V2_MAX_ACCESS_UNIT_BYTES)
    return { decoded: null, rejection: "v3-header" };
  if (!validateWssTopology(topology)) return { decoded: null, rejection: "topology" };
  if (header.shardIndex >= header.laneCount) return { decoded: null, rejection: "logical-lane" };
  if (bytes.byteLength !== V3_HEADER_BYTES + expected)
    return { decoded: null, rejection: "v3-length" };
  return { decoded: { header, payload: bytes.subarray(V3_HEADER_BYTES) }, rejection: null };
}

export function decodeMediaShardV3(packet) {
  return inspectMediaShardV3(packet).decoded;
}

export function classifyMediaShardV3(packet, options = {}) {
  const inspected = inspectMediaShardV3(packet);
  if (inspected.rejection) return inspected.rejection;
  const { header } = inspected.decoded;
  const topology = options.topology;
  if (topology && (header.laneCount !== topology.laneCount ||
      header.dataShards !== topology.dataShards || header.parityShards !== topology.parityShards))
    return "topology";
  if (options.lane !== undefined &&
      (!Number.isInteger(options.lane) || options.lane < 0 || options.lane >= header.laneCount))
    return "physical-lane";
  if (Number.isInteger(options.lane) && header.shardIndex !== options.lane) return "logical-lane";
  if (Number.isInteger(options.generation) && header.generation < options.generation) return "generation";
  return null;
}

function reconstructV3(entry) {
  const { header, shards } = entry;
  const missingData = Array.from({ length: header.dataShards }, (_, index) => index).filter(index => !shards[index]);
  if (missingData.length > header.parityShards) return null;
  if (missingData.length === 1) {
    const missing = missingData[0];
    const p = shards[header.dataShards] ?? null;
    const q = header.parityShards === 2 ? shards[header.dataShards + 1] ?? null : null;
    if (!p && !q) return null;
    const recovered = (p ?? q).slice();
    for (let byte = 0; byte < recovered.byteLength; byte += 1) {
      for (let other = 0; other < header.dataShards; other += 1) {
        if (other === missing) continue;
        recovered[byte] ^= p
          ? shards[other][byte]
          : gfMultiply(pqCoefficient(other), shards[other][byte]);
      }
      if (!p) {
        const value = gfDivide(recovered[byte], pqCoefficient(missing));
        if (value === null) return null;
        recovered[byte] = value;
      }
    }
    shards[missing] = recovered;
  } else if (missingData.length === 2) {
    const p = shards[header.dataShards] ?? null;
    const q = shards[header.dataShards + 1] ?? null;
    if (!p || !q) return null;
    const [first, second] = missingData;
    const firstCoefficient = pqCoefficient(first);
    const secondCoefficient = pqCoefficient(second);
    const denominator = firstCoefficient ^ secondCoefficient;
    const firstRecovered = new Uint8Array(p.byteLength);
    const secondRecovered = new Uint8Array(p.byteLength);
    for (let byte = 0; byte < p.byteLength; byte += 1) {
      let pResidual = p[byte];
      let qResidual = q[byte];
      for (let other = 0; other < header.dataShards; other += 1) {
        if (other === first || other === second) continue;
        pResidual ^= shards[other][byte];
        qResidual ^= gfMultiply(pqCoefficient(other), shards[other][byte]);
      }
      const value = gfDivide(qResidual ^ gfMultiply(secondCoefficient, pResidual), denominator);
      if (value === null) return null;
      firstRecovered[byte] = value;
      secondRecovered[byte] = pResidual ^ value;
    }
    shards[first] = firstRecovered;
    shards[second] = secondRecovered;
  }
  const data = new Uint8Array(header.originalLength);
  let offset = 0;
  for (let index = 0; index < header.dataShards && offset < data.byteLength; index += 1) {
    const count = Math.min(shards[index].byteLength, data.byteLength - offset);
    data.set(shards[index].subarray(0, count), offset); offset += count;
  }
  return { ...header, data, recovered: missingData.length > 0 };
}

export class VariableParityFrameReassembler {
  constructor({ topology, now = () => performance.now(), ttlMs = V2_FRAME_TTL_MS } = {}) {
    if (!validateWssTopology(topology)) throw new RangeError("invalid WSS topology");
    this.topology = Object.freeze({ ...topology }); this.now = now; this.ttlMs = ttlMs;
    this.generation = null; this.waitingForKey = true; this.pending = new Map(); this.completed = new Set();
    this.completionTimes = new Map(); this.firstShardTimes = new Map();
    this.pendingBytes = 0; this.highestSequence = Array(topology.laneCount).fill(0); this.highestReconstructed = 0;
    this.lastProgressAtMs = Array(topology.laneCount).fill(this.now());
    this.stats = {
      recoveredFrames: 0,
      expiredFrames: 0,
      completionMissingByLane: Array(topology.laneCount).fill(0),
      lateShardCountByLane: Array(topology.laneCount).fill(0),
      lateShardDelayMsTotalByLane: Array(topology.laneCount).fill(0),
      lateShardDelayMsMaxByLane: Array(topology.laneCount).fill(0),
    };
    this.expired = []; this.lateShardAges = []; this.lastCompletionAgeMs = 0;
    this.immediateAckPending = false;
  }
  push(packet, lane, eventTimestampMs = null, edgeReceivedAtEpochMs = null, hostSentAtEpochMs = null,
      hostRoiTrace = null) {
    const decoded = decodeMediaShardV3(packet);
    if (!decoded || !Number.isInteger(lane) || lane < 0 || lane >= this.topology.laneCount ||
        !this.matches(decoded.header)) return null;
    const { header, payload } = decoded;
    if (this.generation !== null && header.generation < this.generation) return null;
    if (this.generation !== header.generation) this.reset(header.generation);
    // Lane ACKs describe validated transport delivery, not whether this shard
    // was still needed for reconstruction. A delayed lane must be able to
    // acknowledge its recovery IDR even after parity completed that frame.
    const sequenceAdvanced = header.sequence > this.highestSequence[lane];
    this.highestSequence[lane] = Math.max(this.highestSequence[lane], header.sequence);
    if (sequenceAdvanced) this.lastProgressAtMs[lane] = this.now();
    const id = `${header.generation}:${header.sequence}`;
    if (this.completed.has(id)) {
      if (sequenceAdvanced) {
        this.immediateAckPending = true;
        const completedAt = this.completionTimes.get(id);
        if (completedAt !== undefined) {
          const delayMs = Math.max(0, this.now() - completedAt);
          const firstShardAt = this.firstShardTimes.get(id);
          if (firstShardAt !== undefined) this.lateShardAges.push(Math.max(0, this.now() - firstShardAt));
          this.stats.lateShardCountByLane[lane] += 1;
          this.stats.lateShardDelayMsTotalByLane[lane] += delayMs;
          this.stats.lateShardDelayMsMaxByLane[lane] =
            Math.max(this.stats.lateShardDelayMsMaxByLane[lane], delayMs);
        }
      }
      return null;
    }
    let entry = this.pending.get(id);
    if (!entry) {
      const createdAt = this.now();
      const firstShardEventAtMs = validatedEventTimestamp(eventTimestampMs, createdAt);
      entry = {
        header,
        shards: Array(this.topology.laneCount),
        physicalLanes: Array(this.topology.laneCount).fill(false),
        received: 0,
        createdAt,
        firstShardEventAtMs,
        firstShardEdgeAtEpochMs: Number.isFinite(edgeReceivedAtEpochMs) ? edgeReceivedAtEpochMs : null,
        firstShardHostSentAtEpochMs: Number.isFinite(hostSentAtEpochMs) ? hostSentAtEpochMs : null,
        hostRoiTrace: hostRoiTrace && typeof hostRoiTrace === "object" ? { ...hostRoiTrace } : null,
      };
      this.pending.set(id, entry);
    }
    const logicalShard = header.shardIndex;
    if (!this.sameFrame(entry.header, header) || entry.shards[logicalShard] || entry.physicalLanes[lane]) return null;
    entry.shards[logicalShard] = payload;
    entry.physicalLanes[lane] = true;
    entry.received += 1;
    this.pendingBytes += payload.byteLength;
    if (entry.received < this.topology.dataShards || (this.waitingForKey && !header.key)) {
      this.expire();
      return null;
    }
    const missingAtCompletion = entry.physicalLanes.map(received => !received);
    const reconstructingShardAtMs = this.now();
    const completingShardEventAtMs = validatedEventTimestamp(eventTimestampMs, reconstructingShardAtMs);
    const frame = reconstructV3(entry);
    if (!frame) {
      this.expire();
      return null;
    }
    frame.frameTrace = {
      sequence: frame.sequence,
      encodedBytes: frame.originalLength,
      key: frame.key,
      recovered: frame.recovered,
      firstShardAtMs: entry.createdAt,
      firstShardEdgeAtEpochMs: entry.firstShardEdgeAtEpochMs,
      firstShardHostSentAtEpochMs: entry.firstShardHostSentAtEpochMs,
      firstShardEventAtMs: entry.firstShardEventAtMs,
      firstShardHandlerAtMs: entry.createdAt,
      firstShardQueueDelayMs: entry.firstShardEventAtMs === null ? null : entry.createdAt - entry.firstShardEventAtMs,
      reconstructingShardAtMs,
      completingShardEventAtMs,
      completingShardHandlerAtMs: reconstructingShardAtMs,
      completingShardQueueDelayMs: completingShardEventAtMs === null ? null :
        reconstructingShardAtMs - completingShardEventAtMs,
      reconstructedAtMs: this.now(),
      ...(entry.hostRoiTrace ? {
        roiReason: entry.hostRoiTrace.reason,
        roiChangedCtbs: entry.hostRoiTrace.changedCtbs,
        roiTotalCtbs: entry.hostRoiTrace.totalCtbs,
      } : {}),
    };
    const completedAt = this.now();
    for (let index = 0; index < missingAtCompletion.length; index += 1) {
      if (missingAtCompletion[index]) this.stats.completionMissingByLane[index] += 1;
    }
    this.delete(id); this.completed.add(id); this.completionTimes.set(id, completedAt);
    this.firstShardTimes.set(id, entry.createdAt);
    this.expire();
    this.pruneCompleted();
    if (frame.key) { this.waitingForKey = false; this.dropBefore(frame.sequence); }
    this.lastCompletionAgeMs = Math.max(0, this.now() - entry.createdAt);
    if (frame.recovered) this.stats.recoveredFrames += 1;
    this.highestReconstructed = Math.max(this.highestReconstructed, frame.sequence);
    return frame;
  }
  takeImmediateAck() { const pending = this.immediateAckPending; this.immediateAckPending = false; return pending; }
  takeLateShardAges() { return this.lateShardAges.splice(0); }
  noteLaneConnected(lane) {
    if (!Number.isInteger(lane) || lane < 0 || lane >= this.topology.laneCount) return false;
    this.lastProgressAtMs[lane] = this.now();
    return true;
  }
  matches(header) { return header.laneCount === this.topology.laneCount &&
    header.dataShards === this.topology.dataShards && header.parityShards === this.topology.parityShards; }
  sameFrame(left, right) { return left.gop === right.gop && left.originalLength === right.originalLength &&
    left.laneCount === right.laneCount && left.dataShards === right.dataShards && left.parityShards === right.parityShards; }
  expire() {
    const now = this.now(); const expired = [];
    for (const [id, entry] of this.pending) if (now - entry.createdAt > this.ttlMs) {
      this.delete(id); this.completed.add(id); this.stats.expiredFrames += 1;
      expired.push({ generation: entry.header.generation, sequence: entry.header.sequence, key: entry.header.key });
    }
    while (this.pending.size > V2_MAX_PENDING_FRAMES || this.pendingBytes > V2_MAX_REASSEMBLY_BYTES) {
      const id = this.pending.keys().next().value; const entry = this.pending.get(id);
      this.delete(id); this.completed.add(id); this.stats.expiredFrames += 1;
      expired.push({ generation: entry.header.generation, sequence: entry.header.sequence, key: entry.header.key });
    }
    this.pruneCompleted();
    this.expired.push(...expired); return expired;
  }
  takeExpired() { return this.expired.splice(0); }
  waitForKeyframe() { this.waitingForKey = true; this.pending.clear(); this.completed.clear(); this.completionTimes.clear(); this.firstShardTimes.clear(); this.pendingBytes = 0;
    this.lateShardAges.length = 0; this.immediateAckPending = false; }
  setTtlMs(ttlMs) { if (Number.isFinite(ttlMs) && ttlMs > 0) this.ttlMs = ttlMs; }
  dropBefore(sequence) { for (const [id, entry] of this.pending) if (entry.header.sequence < sequence) this.delete(id); }
  delete(id) { const entry = this.pending.get(id); if (!entry) return;
    for (const shard of entry.shards) if (shard) this.pendingBytes -= shard.byteLength; this.pending.delete(id); }
  pruneCompleted() {
    while (this.completed.size > V3_COMPLETED_FRAME_WINDOW) {
      const id = this.completed.values().next().value;
      this.completed.delete(id);
      this.completionTimes.delete(id);
      this.firstShardTimes.delete(id);
    }
  }
  reset(generation = null) { this.generation = generation; this.waitingForKey = true; this.pending.clear();
    this.completed.clear(); this.completionTimes.clear(); this.firstShardTimes.clear(); this.expired.length = 0; this.lateShardAges.length = 0; this.pendingBytes = 0; this.highestSequence.fill(0);
    this.lastProgressAtMs.fill(this.now());
    this.highestReconstructed = 0; this.lastCompletionAgeMs = 0; this.immediateAckPending = false; }
}

function validatedEventTimestamp(eventTimestampMs, handlerAtMs) {
  return Number.isFinite(eventTimestampMs) && eventTimestampMs >= 0 && eventTimestampMs <= handlerAtMs
    ? eventTimestampMs
    : null;
}

export function decodeMediaShardV2(packet) {
  const bytes = asBytes(packet);
  if (!bytes || bytes.byteLength < V2_HEADER_BYTES || bytes.byteLength > V2_MAX_PACKET_BYTES) return null;
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const header = {
    version: view.getUint8(0), codec: view.getUint8(1), key: Boolean(view.getUint8(2) & 1),
    shardIndex: view.getUint8(3), generation: view.getUint32(4), gop: view.getUint32(8),
    sequence: view.getUint32(12), timestamp90k: view.getUint32(16), originalLength: view.getUint32(20),
  };
  const expected = Math.ceil(header.originalLength / V2_DATA_SHARDS);
  if (header.version !== V2_VERSION || !Object.values(CODEC).includes(header.codec) ||
      header.shardIndex >= V2_SHARD_COUNT || !header.originalLength ||
      header.originalLength > V2_MAX_ACCESS_UNIT_BYTES || bytes.byteLength !== V2_HEADER_BYTES + expected) return null;
  return { header, payload: bytes.slice(V2_HEADER_BYTES) };
}

function reconstructV2(entry) {
  const { header, shards } = entry;
  const missing = [0, 1, 2].filter((index) => !shards[index]);
  if (missing.length > 1 || (missing.length === 1 && !shards[3])) return null;
  if (missing.length === 1) {
    const index = missing[0];
    const recovered = shards[3].slice();
    for (let other = 0; other < V2_DATA_SHARDS; other += 1) {
      if (other === index) continue;
      for (let byte = 0; byte < recovered.byteLength; byte += 1) recovered[byte] ^= shards[other][byte];
    }
    shards[index] = recovered;
  }
  const data = new Uint8Array(header.originalLength);
  let offset = 0;
  for (let index = 0; index < V2_DATA_SHARDS && offset < data.byteLength; index += 1) {
    const count = Math.min(shards[index].byteLength, data.byteLength - offset);
    data.set(shards[index].subarray(0, count), offset); offset += count;
  }
  return { ...header, data, recovered: missing.length === 1 };
}

export class ParityFrameReassembler {
  constructor({ now = () => performance.now(), ttlMs = V2_FRAME_TTL_MS } = {}) {
    this.now = now; this.ttlMs = ttlMs; this.generation = null; this.waitingForKey = true;
    this.pending = new Map(); this.completed = new Set(); this.pendingBytes = 0; this.highestSequence = Array(4).fill(0); this.highestReconstructed = 0;
    this.stats = { recoveredFrames: 0, expiredFrames: 0 };
    this.expired = [];
  }
  push(packet, stripe) {
    const decoded = decodeMediaShardV2(packet);
    if (!decoded || decoded.header.shardIndex !== stripe) return null;
    const { header, payload } = decoded;
    if (this.generation !== null && header.generation < this.generation) return null;
    if (this.generation !== header.generation) this.reset(header.generation);
    if (this.waitingForKey && !header.key) return null;
    const id = `${header.generation}:${header.sequence}`;
    if (this.completed.has(id)) return null;
    let entry = this.pending.get(id);
    if (!entry) {
      entry = { header, shards: Array(4), received: 0, createdAt: this.now() };
      this.pending.set(id, entry);
    }
    if (entry.header.gop !== header.gop || entry.header.originalLength !== header.originalLength || entry.shards[stripe]) return null;
    entry.shards[stripe] = payload; entry.received += 1; this.pendingBytes += payload.byteLength;
    this.highestSequence[stripe] = Math.max(this.highestSequence[stripe], header.sequence);
    if (entry.received < V2_DATA_SHARDS) {
      this.expire();
      return null;
    }
    const reconstructingShardAtMs = this.now();
    const frame = reconstructV2(entry);
    if (!frame) {
      this.expire();
      return null;
    }
    frame.frameTrace = {
      sequence: frame.sequence,
      encodedBytes: frame.originalLength,
      key: frame.key,
      recovered: frame.recovered,
      firstShardAtMs: entry.createdAt,
      reconstructingShardAtMs,
      reconstructedAtMs: this.now(),
    };
    this.delete(id);
    this.completed.add(id);
    this.expire();
    while (this.completed.size > 32) this.completed.delete(this.completed.values().next().value);
    if (frame.key) { this.waitingForKey = false; this.dropBefore(frame.sequence); }
    this.lastCompletionAgeMs = Math.max(0, this.now() - entry.createdAt);
    if (frame.recovered) this.stats.recoveredFrames += 1;
    this.highestReconstructed = Math.max(this.highestReconstructed, frame.sequence);
    return frame;
  }
  expire() {
    const now = this.now();
    const expired = [];
    for (const [id, entry] of this.pending) {
      if (now - entry.createdAt > this.ttlMs) {
        this.delete(id); this.completed.add(id); this.stats.expiredFrames += 1;
        expired.push({ generation: entry.header.generation, sequence: entry.header.sequence, key: entry.header.key });
      }
    }
    while (this.pending.size > V2_MAX_PENDING_FRAMES || this.pendingBytes > V2_MAX_REASSEMBLY_BYTES) {
      const id = this.pending.keys().next().value; const entry = this.pending.get(id); this.delete(id); this.completed.add(id); this.stats.expiredFrames += 1;
      expired.push({ generation: entry.header.generation, sequence: entry.header.sequence, key: entry.header.key });
    }
    while (this.completed.size > 32) this.completed.delete(this.completed.values().next().value);
    this.expired.push(...expired);
    return expired;
  }
  takeExpired() { return this.expired.splice(0); }
  waitForKeyframe() {
    this.waitingForKey = true;
    this.pending.clear(); this.completed.clear(); this.pendingBytes = 0;
  }
  setTtlMs(ttlMs) {
    if (Number.isFinite(ttlMs) && ttlMs > 0) this.ttlMs = ttlMs;
  }
  dropBefore(sequence) { for (const [id, entry] of this.pending) if (entry.header.sequence < sequence) this.delete(id); }
  delete(id) { const entry = this.pending.get(id); if (!entry) return; for (const shard of entry.shards) if (shard) this.pendingBytes -= shard.byteLength; this.pending.delete(id); }
  reset(generation = null) { this.generation = generation; this.waitingForKey = true; this.pending.clear(); this.completed.clear(); this.expired.length = 0; this.pendingBytes = 0; this.highestSequence.fill(0); this.highestReconstructed = 0; this.lastCompletionAgeMs = 0; }
}

export class OrderedFrameRelease {
  constructor({ now = () => performance.now(), ttlMs = V2_FRAME_TTL_MS } = {}) {
    this.now = now; this.ttlMs = ttlMs; this.reset();
  }
  push(frame) {
    if (!frame) return [];
    if (this.generation !== null && frame.generation < this.generation) return [];
    if (this.generation !== frame.generation) this.reset(frame.generation);
    // A fresh IDR is a valid dependency reset. Let it leap over a missing
    // delta immediately instead of waiting for the 120 ms gap deadline.
    if (frame.key && (this.waitingForKey || this.expected === null || frame.sequence !== this.expected)) {
      this.ready.clear(); this.expected = frame.sequence; this.waitingForKey = false; this.gapSince = null;
    }
    if (this.waitingForKey || this.expected === null || frame.sequence < this.expected) return [];
    if (!this.ready.has(frame.sequence)) this.ready.set(frame.sequence, frame);
    return this.drain();
  }
  drain() {
    const frames = [];
    while (this.ready.has(this.expected)) {
      frames.push(this.ready.get(this.expected)); this.ready.delete(this.expected);
      this.expected = (this.expected + 1) >>> 0; this.gapSince = null;
      if (this.lost.has(this.expected)) { this.breakDependency(); break; }
    }
    if (this.ready.size && this.gapSince === null) this.gapSince = this.now();
    while (this.ready.size > V2_MAX_PENDING_FRAMES) {
      this.breakDependency();
    }
    return frames;
  }
  expire() {
    if (this.gapSince === null || this.now() - this.gapSince <= this.ttlMs) return false;
    this.breakDependency();
    return true;
  }
  setTtlMs(ttlMs) { if (Number.isFinite(ttlMs) && ttlMs > 0) this.ttlMs = ttlMs; }
  markLost(sequence) {
    sequence >>>= 0;
    if (this.waitingForKey || this.expected === null || sequence < this.expected) return false;
    if (sequence === this.expected) { this.breakDependency(); return true; }
    this.lost.add(sequence);
    while (this.lost.size > V2_MAX_PENDING_FRAMES * 2) this.lost.delete(this.lost.values().next().value);
    return false;
  }
  breakDependency() {
    this.ready.clear(); this.lost.clear(); this.waitingForKey = true; this.expected = null; this.gapSince = null;
    this.expiredFrames += 1; this.dependencyLosses += 1;
  }
  reset(generation = null) {
    this.generation = generation; this.expected = null; this.waitingForKey = true;
    this.ready = new Map(); this.lost = new Set(); this.gapSince = null; this.expiredFrames = 0; this.dependencyLosses = 0;
  }
}
