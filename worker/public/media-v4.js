// WSS media protocol v4 for the receiver; the Rust relay (relay-rs) forwards it.
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
// Retries wait the backoff plus or minus this fraction, so clients that lost the relay together
// do not return together.
export const V4_BACKOFF_JITTER_FRACTION = 0.2;
// Frames queued in a decoder before it counts as wedged: the viewer drops to the next keyframe.
// The host keeps at most 12 frames in flight (wss_v4.rs in-flight window), so a healthy decoder
// stays far below this.
export const V4_DECODE_BACKLOG_LIMIT_FRAMES = 30;
const KNOWN_FLAGS = V4_FLAG_KEY | V4_FLAG_REFINE;
const REASON = /^[A-Za-z0-9_.:-]{1,64}$/;

function uint32(value) { return Number.isInteger(value) && value >= 0 && value <= 0xffff_ffff; }

function bytesOf(value) {
  if (value instanceof ArrayBuffer) return new Uint8Array(value);
  if (ArrayBuffer.isView(value)) return new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
  return null;
}

/** Identify Annex B video from NAL headers, ignoring leading AUD/SEI NALs. */
export function detectAnnexBCodec(value, { key = true } = {}) {
  const bytes = bytesOf(value);
  if (!bytes) return null;
  let hevcSlice = false;
  for (let i = 0; i + 3 < bytes.length; i++) {
    if (bytes[i] !== 0 || bytes[i + 1] !== 0) continue;
    const startBytes = bytes[i + 2] === 1 ? 3
      : bytes[i + 2] === 0 && bytes[i + 3] === 1 ? 4 : 0;
    if (!startBytes) continue;
    const nalStart = i + startBytes;
    i = nalStart - 1;
    if (nalStart >= bytes.length || (bytes[nalStart] & 0x80)) continue;
    const avcType = bytes[nalStart] & 0x1f;
    const hevcType = (bytes[nalStart] >> 1) & 0x3f;
    // Base-layer HEVC has a two-byte header and a nonzero temporal_id_plus1.
    // This also keeps an AVC slice (0x41) from looking like an HEVC VPS.
    const hevcHeader = nalStart + 1 < bytes.length &&
      (bytes[nalStart] & 1) === 0 && (bytes[nalStart + 1] & 0xf8) === 0 &&
      (bytes[nalStart + 1] & 7) !== 0;
    if (avcType === 5 || avcType === 7) return "h264";
    if (hevcHeader && ((hevcType >= 19 && hevcType <= 21) ||
        (hevcType >= 32 && hevcType <= 34))) return "hevc";
    if (!key && avcType === 1) return "h264";
    if (!key && hevcHeader && hevcType <= 9) {
      if (avcType !== 6) return "hevc";
      hevcSlice = true;
      continue;
    }
    // HEVC IDR type 20 overlaps AVC PPS type 8; prefer the HEVC header.
    if (avcType === 8) return "h264";
  }
  // AVC SEI can resemble an HEVC slice; the actual AVC slice is decisive.
  return hevcSlice ? "hevc" : null;
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

/** The wait before a retry: `backoffMs` spread by the jitter fraction. `random` returns [0, 1). */
export function jitteredBackoffMsV4(backoffMs, random = Math.random) {
  return Math.round(backoffMs * (1 + (random() * 2 - 1) * V4_BACKOFF_JITTER_FRACTION));
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

/** Receiver status text for v4 (never "waiting for stream" when static). */
export function wssV4StatusLabel({ socketOpen = false, reconnecting = false, framesPresented = 0,
  lastFrameAtMs = null, nowMs = 0, staticAfterMs = 1_000 } = {}) {
  if (!socketOpen) return reconnecting ? "reconnecting" : "connecting";
  if (framesPresented === 0) return "starting";
  if (lastFrameAtMs !== null && nowMs - lastFrameAtMs >= staticAfterMs) return "paused-static";
  return "connected";
}
