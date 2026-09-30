// WSS media protocol v4, shared by the relay (worker/src) and the receiver.
//
// One media socket per session. Host -> client: binary frames with a
// 28-byte big-endian header followed by one Annex-B access unit:
//   magic u32 "BPV4" | version u8 = 4 | flags u8 (bit0 key, bit1 refine) |
//   codec u8 (1 HEVC, 2 H.264) | reserved u8 = 0 | generation u32 | seq u32 |
//   captureTsUs u64 | width u16 | height u16
// Client -> host: small JSON text on the same socket — per-frame ACKs,
// keyframe requests and heartbeats. The relay answers heartbeats itself.
// The contract lives in fixtures/wss-v4-contract.json and is shared with
// host/src/wss_v4.rs.

export const V4_VERSION = 4;
export const V4_MAGIC = 0x42505634;
export const V4_HEADER_BYTES = 28;
export const V4_MAX_PAYLOAD_BYTES = 8 * 1024 * 1024;
export const V4_MAX_PACKET_BYTES = V4_HEADER_BYTES + V4_MAX_PAYLOAD_BYTES;
export const V4_MAX_CONTROL_BYTES = 512;
export const V4_FLAG_KEY = 0x01;
export const V4_FLAG_REFINE = 0x02;
export const V4_HEARTBEAT_MS = 15_000;
export const V4_BACKOFF_MIN_MS = 250;
export const V4_BACKOFF_MAX_MS = 5_000;
export const V4_KEYFRAME_REQUEST_INTERVAL_MS = 250;
const KNOWN_FLAGS = V4_FLAG_KEY | V4_FLAG_REFINE;
const REASON = /^[A-Za-z0-9_.:-]{1,64}$/;

function uint32(value) { return Number.isInteger(value) && value >= 0 && value <= 0xffff_ffff; }

function bytesOf(value) {
  if (value instanceof ArrayBuffer) return new Uint8Array(value);
  if (ArrayBuffer.isView(value)) return new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
  return null;
}

/** Parse one v4 packet; returns null for anything the contract rejects. */
export function parseFrameV4(packet) {
  const bytes = bytesOf(packet);
  if (!bytes || bytes.byteLength <= V4_HEADER_BYTES || bytes.byteLength > V4_MAX_PACKET_BYTES) return null;
  const view = new DataView(bytes.buffer, bytes.byteOffset, V4_HEADER_BYTES);
  const flags = view.getUint8(5);
  const codec = view.getUint8(6);
  if (view.getUint32(0) !== V4_MAGIC || view.getUint8(4) !== V4_VERSION ||
      (flags & ~KNOWN_FLAGS) !== 0 || (codec !== 1 && codec !== 2) || view.getUint8(7) !== 0) return null;
  return {
    key: (flags & V4_FLAG_KEY) !== 0,
    refine: (flags & V4_FLAG_REFINE) !== 0,
    codec,
    generation: view.getUint32(8),
    seq: view.getUint32(12),
    captureTsUs: view.getBigUint64(16),
    width: view.getUint16(24),
    height: view.getUint16(26),
    data: bytes.subarray(V4_HEADER_BYTES),
  };
}

export function encodeFrameV4({ key = false, refine = false, codec, generation, seq, captureTsUs = 0n,
  width = 0, height = 0, data }) {
  const payload = bytesOf(data);
  if (!payload || payload.byteLength === 0 || payload.byteLength > V4_MAX_PAYLOAD_BYTES) {
    throw new RangeError("v4 payload size out of range");
  }
  const packet = new Uint8Array(V4_HEADER_BYTES + payload.byteLength);
  const view = new DataView(packet.buffer);
  view.setUint32(0, V4_MAGIC);
  view.setUint8(4, V4_VERSION);
  view.setUint8(5, (key ? V4_FLAG_KEY : 0) | (refine ? V4_FLAG_REFINE : 0));
  view.setUint8(6, codec);
  view.setUint32(8, generation >>> 0);
  view.setUint32(12, seq >>> 0);
  view.setBigUint64(16, BigInt(captureTsUs));
  view.setUint16(24, width);
  view.setUint16(26, height);
  packet.set(payload, V4_HEADER_BYTES);
  return packet;
}

function utf8Length(text) { return new TextEncoder().encode(text).byteLength; }

/**
 * Validate a browser -> host control message. Returns the parsed message or
 * null. Mirrored by host/src/wss_v4.rs `parse_client_message`.
 */
export function validateClientMessageV4(text) {
  if (typeof text !== "string" || utf8Length(text) > V4_MAX_CONTROL_BYTES) return null;
  let message;
  try { message = JSON.parse(text); } catch { return null; }
  if (!message || typeof message !== "object" || Array.isArray(message)) return null;
  const keys = Object.keys(message);
  const only = allowed => keys.every(key => allowed.includes(key));
  switch (message.type) {
    case "ack":
      if (!only(["type", "generation", "seq", "recvTsMs", "decodedSeq"]) ||
          !uint32(message.generation) || !uint32(message.seq) ||
          typeof message.recvTsMs !== "number" || !Number.isFinite(message.recvTsMs) || message.recvTsMs < 0 ||
          (message.decodedSeq !== undefined && !uint32(message.decodedSeq))) return null;
      return message;
    case "keyframe":
      return only(["type", "reason"]) && typeof message.reason === "string" && REASON.test(message.reason)
        ? message : null;
    case "ping":
      return only(["type"]) ? message : null;
    default:
      return null;
  }
}

/** Host -> relay text on an established v4 media socket: heartbeat only. */
export function validateHostTextV4(text) {
  if (typeof text !== "string" || utf8Length(text) > V4_MAX_CONTROL_BYTES) return null;
  let message;
  try { message = JSON.parse(text); } catch { return null; }
  if (!message || typeof message !== "object") return null;
  if (message.type === "ping") return message;
  if (message.type === "hello" && message.role === "media-host") return message;
  return null;
}

export function ackMessageV4({ generation, seq, recvTsMs, decodedSeq }) {
  const message = { type: "ack", generation: generation >>> 0, seq: seq >>> 0,
    recvTsMs: Math.max(0, Math.round(recvTsMs * 10) / 10) };
  if (uint32(decodedSeq)) message.decodedSeq = decodedSeq;
  return JSON.stringify(message);
}

export function keyframeMessageV4(reason) {
  const safe = String(reason || "receiver").replace(/[^A-Za-z0-9_.:-]/g, "-").slice(0, 64) || "receiver";
  return JSON.stringify({ type: "keyframe", reason: safe });
}

/** Reconnect backoff: 250 ms doubling to 5 s. */
export function nextBackoffMsV4(previousMs) {
  if (!Number.isFinite(previousMs) || previousMs < V4_BACKOFF_MIN_MS) return V4_BACKOFF_MIN_MS;
  return Math.min(V4_BACKOFF_MAX_MS, previousMs * 2);
}

/**
 * Receive-side ordering gate. Every accepted frame must be decoded, in
 * order. A sequence gap, a new generation without a keyframe, or a decoder
 * error discards deltas until the next keyframe and asks for one.
 */
export class V4ReceiveGate {
  constructor() { this.reset(); }

  reset(reason = "connect") {
    this.generation = null;
    this.expectedSeq = null;
    this.waitingForKey = true;
    this.pendingReason = reason;
  }

  /** Decoder failure or recovery: drop deltas until the next keyframe. */
  invalidate(reason) {
    this.waitingForKey = true;
    this.pendingReason = reason;
  }

  /**
   * @returns {{decode: boolean, requestKeyframe: string|null}}
   */
  accept(frame) {
    if (frame.key) {
      this.generation = frame.generation;
      this.expectedSeq = (frame.seq + 1) >>> 0;
      this.waitingForKey = false;
      this.pendingReason = null;
      return { decode: true, requestKeyframe: null };
    }
    let reason = null;
    if (this.waitingForKey) reason = this.pendingReason || "awaiting-keyframe";
    else if (frame.generation !== this.generation) reason = "generation-change";
    else if (frame.seq !== this.expectedSeq) reason = "sequence-gap";
    if (reason) {
      this.waitingForKey = true;
      this.pendingReason = reason;
      return { decode: false, requestKeyframe: reason };
    }
    this.expectedSeq = (frame.seq + 1) >>> 0;
    return { decode: true, requestKeyframe: null };
  }
}

/**
 * Presentation policy: every frame is decoded, but while the decoder is
 * behind (several chunks still queued) intermediate outputs are not painted.
 * A frame is still painted at least every `maxSkipMs` so a persistently
 * slow decoder never freezes the picture.
 */
export function shouldPresentDecodedV4({ decodeQueueSize = 0, msSinceLastPresent = Infinity,
  maxQueued = 1, maxSkipMs = 100 } = {}) {
  return decodeQueueSize <= maxQueued || msSinceLastPresent >= maxSkipMs;
}

/**
 * Production WSS prefers protocol 4 whenever the host advertises it. Stream
 * Lab runs and explicit topology requests keep the v2/v3 lane protocols
 * unless the lab run explicitly opts in (`wssProtocol=4`).
 */
export function preferWssProtocolV4({ lab = false, labRequestsV4 = false, requestedTopology = null,
  hostCapabilities = null, capabilitiesCurrent = true } = {}) {
  if ((lab && !labRequestsV4) || requestedTopology || !capabilitiesCurrent) return false;
  return Array.isArray(hostCapabilities?.mediaProtocols) && hostCapabilities.mediaProtocols.includes(V4_VERSION);
}

/** Lab-only opt-in: `?labRun=...&wssProtocol=4`. Ignored outside lab runs. */
export function labRequestsWssProtocolV4(searchParams, labRun) {
  return Boolean(labRun) && searchParams?.get?.("wssProtocol") === String(V4_VERSION);
}

/** Receiver status text for v4 (never "waiting for stream" when static). */
export function wssV4StatusLabel({ socketOpen = false, reconnecting = false, framesPresented = 0,
  lastFrameAtMs = null, nowMs = 0, staticAfterMs = 1_000 } = {}) {
  if (!socketOpen) return reconnecting ? "reconnecting" : "connecting";
  if (framesPresented === 0) return "starting";
  if (lastFrameAtMs !== null && nowMs - lastFrameAtMs >= staticAfterMs) return "paused-static";
  return "connected";
}
