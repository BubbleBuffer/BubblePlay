export const STREAM_LAB_ASSET_BUILD = "wss-v4-bench2-hevc-tiles-bpip4-v16";
export const CODEC = Object.freeze({ HEVC: 1, H264: 2 });
export const STREAM_LAB_COMPATIBILITY = Object.freeze({
  schemaVersion: 1,
  id: STREAM_LAB_ASSET_BUILD,
  receiverBuild: STREAM_LAB_ASSET_BUILD,
  mediaProtocol: 4,
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
    "signalingRecycle:v1",
    "networkRouteProbe:v2",
    "wssImmediatePresentation:v1",
  ]),
});
export const HYBRID_FRAME_MAGIC = 0x42504831;
export const HYBRID_FRAME_VERSION = 1;

export function createSignalingRecycleDiagnostic(state = {}) {
  const counter = value => Number.isSafeInteger(value) && value >= 0 ? value : null;
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
    mediaSocketOpen: state.mediaSocketOpen === true,
    mediaGeneration: counter(state.mediaGeneration),
    reconstructed: counter(state.reconstructed),
    presented: counter(state.presented),
    receivedPackets: counter(state.receivedPackets),
    recoveryState,
    renderedAgeMs,
    online: state.online === true,
  };
}

function asBytes(value) {
  if (value instanceof Uint8Array) return value;
  if (value instanceof ArrayBuffer) return new Uint8Array(value);
  if (ArrayBuffer.isView(value)) return new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
  return null;
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

/** A connected WebRTC peer is only abandoned for WSS when no media has
 * arrived. The native `<video>` path presents frames through
 * requestVideoFrameCallback independently of encoded-frame callbacks, so
 * both concrete arrival signals must be consulted before falling back. */
export function shouldFallbackToWssOnMissingRtp({
  connected = false,
  framesReceived = 0,
  nativeFramesPresented = 0,
} = {}) {
  if (connected !== true) return false;
  const received = Number.isFinite(framesReceived) ? framesReceived : 0;
  const presented = Number.isFinite(nativeFramesPresented) ? nativeFramesPresented : 0;
  return received <= 0 && presented <= 0;
}
