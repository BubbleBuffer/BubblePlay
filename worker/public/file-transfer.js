// File transfer protocol v1, shared by the relay (worker/src) and the web
// receiver. Intent and wire contract: docs/architecture/file-transfer.md. Shared
// vectors: fixtures/file-transfer-contract.json (also tested from Rust in
// host/src/file_transfer.rs and from Kotlin).
//
// Every file-socket message is binary: a 16-byte header + payload.
//   magic "BPF1" | type u8 | flags u8 = 0 | reserved u16 = 0 |
//   transferId u32 LE | seq u32 LE (DATA chunk index, otherwise 0)

export const FT_MAGIC = [0x42, 0x50, 0x46, 0x31];
export const FT_HEADER_BYTES = 16;
export const FT_MAX_DATA_BYTES = 64 * 1024;
export const FT_MAX_CONTROL_PAYLOAD_BYTES = 4 * 1024;
export const FT_MAX_MESSAGE_BYTES = FT_HEADER_BYTES + FT_MAX_DATA_BYTES;
export const FT_WINDOW_BYTES = 1024 * 1024;
export const FT_ACK_EVERY_BYTES = 256 * 1024;
export const FT_ACK_EVERY_MS = 100;
export const FT_MAX_FILE_BYTES = 2 * 1024 * 1024 * 1024;
export const FT_MAX_BATCH_FILES = 64;
export const FT_MAX_NAME_BYTES = 1024;
export const FT_MAX_MIME_BYTES = 255;
export const FT_MAX_REASON_BYTES = 256;
export const FT_IDLE_CLOSE_MS = 60_000;
/** Largest download the web receiver buffers in memory (no File System Access API). */
export const FT_BLOB_DOWNLOAD_MAX_BYTES = 256 * 1024 * 1024;

export const FT = Object.freeze({
  OFFER: 1, ACCEPT: 2, DATA: 3, ACK: 4, DONE: 5, CANCEL: 6, RESULT: 7, REQUEST: 8,
});
export const FT_TYPE_NAMES = Object.freeze(Object.fromEntries(Object.entries(FT).map(([name, value]) => [value, name])));

const encoder = new TextEncoder();
const strictDecoder = new TextDecoder("utf-8", { fatal: true });

function uint32(value) { return Number.isInteger(value) && value >= 0 && value <= 0xffff_ffff; }
function utf8Bytes(text) { return encoder.encode(text).byteLength; }

function bytesOf(value) {
  if (value instanceof ArrayBuffer) return new Uint8Array(value);
  if (ArrayBuffer.isView(value)) return new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
  return null;
}

/**
 * Decode one packet. Returns `{ ok: true, type, typeName, transferId, seq, payload, json?, total?, sha256? }`
 * or `{ ok: false, reason }` with the fixture's reason codes.
 */
export function decodeFilePacket(message) {
  const bytes = bytesOf(message);
  if (!bytes || bytes.byteLength < FT_HEADER_BYTES) return { ok: false, reason: "short" };
  if (bytes.byteLength > FT_MAX_MESSAGE_BYTES) return { ok: false, reason: "too-large" };
  for (let i = 0; i < 4; i += 1) if (bytes[i] !== FT_MAGIC[i]) return { ok: false, reason: "magic" };
  const type = bytes[4];
  if (!FT_TYPE_NAMES[type]) return { ok: false, reason: "type" };
  if (bytes[5] !== 0) return { ok: false, reason: "flags" };
  if (bytes[6] !== 0 || bytes[7] !== 0) return { ok: false, reason: "reserved" };
  const view = new DataView(bytes.buffer, bytes.byteOffset, FT_HEADER_BYTES);
  const transferId = view.getUint32(8, true);
  const seq = view.getUint32(12, true);
  const payload = bytes.subarray(FT_HEADER_BYTES);
  if (type !== FT.DATA && seq !== 0) return { ok: false, reason: "seq" };
  const packet = { ok: true, type, typeName: FT_TYPE_NAMES[type], transferId, seq, payload };
  switch (type) {
    case FT.ACCEPT:
      return payload.byteLength === 0 ? packet : { ok: false, reason: "payload" };
    case FT.DATA:
      return payload.byteLength > 0 ? packet : { ok: false, reason: "payload" };
    case FT.ACK: {
      if (payload.byteLength !== 8) return { ok: false, reason: "payload" };
      const low = new DataView(payload.buffer, payload.byteOffset, 8).getUint32(0, true);
      const high = new DataView(payload.buffer, payload.byteOffset, 8).getUint32(4, true);
      const total = high * 0x1_0000_0000 + low;
      if (!Number.isSafeInteger(total)) return { ok: false, reason: "payload" };
      return { ...packet, total };
    }
    case FT.DONE:
      return payload.byteLength === 32 ? { ...packet, sha256: payload.slice() } : { ok: false, reason: "payload" };
    default: {
      if (payload.byteLength === 0 || payload.byteLength > FT_MAX_CONTROL_PAYLOAD_BYTES) {
        return { ok: false, reason: "payload" };
      }
      let json;
      try { json = JSON.parse(strictDecoder.decode(payload)); } catch { return { ok: false, reason: "json" }; }
      if (!validControlPayload(type, json)) return { ok: false, reason: "json" };
      return { ...packet, json };
    }
  }
}

function plainObject(value) { return value !== null && typeof value === "object" && !Array.isArray(value); }

export function validDropPoint(point) {
  return plainObject(point) && [point.x, point.y].every(value =>
    Number.isInteger(value) && value >= 0 && value <= 65535) &&
    uint32(point.generation) && point.generation > 0 && uint32(point.viewportRevision);
}

export function validOfferMeta(meta) {
  return plainObject(meta) &&
    typeof meta.name === "string" && meta.name.length > 0 && utf8Bytes(meta.name) <= FT_MAX_NAME_BYTES &&
    Number.isSafeInteger(meta.size) && meta.size >= 0 && meta.size <= FT_MAX_FILE_BYTES &&
    typeof meta.mime === "string" && utf8Bytes(meta.mime) <= FT_MAX_MIME_BYTES &&
    Number.isSafeInteger(meta.mtimeMs) && meta.mtimeMs >= 0 &&
    (meta.purpose === "save" || meta.purpose === "paste" || meta.purpose === "drop") &&
    (meta.purpose === "drop" ? validDropPoint(meta.drop) : meta.drop === undefined) &&
    uint32(meta.batchId) &&
    Number.isInteger(meta.batchCount) && meta.batchCount >= 1 && meta.batchCount <= FT_MAX_BATCH_FILES &&
    Number.isInteger(meta.batchIndex) && meta.batchIndex >= 0 && meta.batchIndex < meta.batchCount;
}

function validControlPayload(type, json) {
  if (!plainObject(json)) return false;
  switch (type) {
    case FT.OFFER: return validOfferMeta(json);
    case FT.CANCEL: return typeof json.reason === "string" && utf8Bytes(json.reason) <= FT_MAX_REASON_BYTES;
    case FT.RESULT:
      return typeof json.ok === "boolean" &&
        (json.error === undefined || json.error === null ||
         (typeof json.error === "string" && utf8Bytes(json.error) <= FT_MAX_REASON_BYTES));
    case FT.REQUEST: return uint32(json.offerId) && uint32(json.id);
    default: return false;
  }
}

export function encodeFilePacket(type, transferId, seq, payload = new Uint8Array(0)) {
  const body = bytesOf(payload) ?? new Uint8Array(0);
  const out = new Uint8Array(FT_HEADER_BYTES + body.byteLength);
  out.set(FT_MAGIC, 0);
  out[4] = type;
  const view = new DataView(out.buffer);
  view.setUint32(8, transferId >>> 0, true);
  view.setUint32(12, type === FT.DATA ? seq >>> 0 : 0, true);
  out.set(body, FT_HEADER_BYTES);
  return out;
}

function jsonPacket(type, transferId, value) {
  return encodeFilePacket(type, transferId, 0, encoder.encode(JSON.stringify(value)));
}

/** Key order matches the Rust encoder so both produce byte-identical packets. */
export function encodeOffer(transferId, meta) {
  return jsonPacket(FT.OFFER, transferId, {
    name: meta.name, size: meta.size, mime: meta.mime, mtimeMs: meta.mtimeMs, purpose: meta.purpose,
    batchId: meta.batchId, batchIndex: meta.batchIndex, batchCount: meta.batchCount,
    ...(meta.drop !== undefined ? { drop: { x: meta.drop.x, y: meta.drop.y,
      generation: meta.drop.generation, viewportRevision: meta.drop.viewportRevision } } : {}),
  });
}
export function encodeAccept(transferId) { return encodeFilePacket(FT.ACCEPT, transferId, 0); }
export function encodeData(transferId, seq, bytes) { return encodeFilePacket(FT.DATA, transferId, seq, bytes); }
export function encodeAck(transferId, total) {
  const payload = new Uint8Array(8);
  const view = new DataView(payload.buffer);
  view.setUint32(0, total % 0x1_0000_0000, true);
  view.setUint32(4, Math.floor(total / 0x1_0000_0000), true);
  return encodeFilePacket(FT.ACK, transferId, 0, payload);
}
export function encodeDone(transferId, sha256) { return encodeFilePacket(FT.DONE, transferId, 0, sha256); }
export function encodeCancel(transferId, reason) { return jsonPacket(FT.CANCEL, transferId, { reason }); }
export function encodeResult(transferId, ok, error) {
  return jsonPacket(FT.RESULT, transferId, error === undefined || error === null ? { ok } : { ok, error });
}
export function encodeRequest(transferId, offerId, id) { return jsonPacket(FT.REQUEST, transferId, { offerId, id }); }

/**
 * Relay check for one file-socket message. `role` is "file-client" or
 * "file-host". The relay is stateless: it validates each packet on its own
 * and enforces direction only for REQUEST (client -> host).
 */
export function relayAcceptsFilePacket(message, role) {
  if (typeof message === "string") return false;
  const packet = decodeFilePacket(message);
  if (!packet.ok) return false;
  if (packet.type === FT.REQUEST && role !== "file-client") return false;
  return true;
}

/** Validates host -> client `file-offer` signaling text (also used by the relay). */
export function validFileOffer(message) {
  return plainObject(message) && message.type === "file-offer" && message.protocol === 1 &&
    uint32(message.offerId) && (message.source === "clipboard" || message.source === "sendto") &&
    Array.isArray(message.items) && message.items.length <= FT_MAX_BATCH_FILES &&
    message.items.every(item => plainObject(item) && uint32(item.id) &&
      typeof item.name === "string" && item.name.length > 0 && utf8Bytes(item.name) <= FT_MAX_NAME_BYTES &&
      Number.isSafeInteger(item.size) && item.size >= 0 && item.size <= FT_MAX_FILE_BYTES);
}

/** Receiver-side state for one incoming file (mirrors `Incoming` in Rust). */
export class FileIncoming {
  constructor(transferId, meta) {
    this.transferId = transferId;
    this.size = meta.size;
    this.nextSeq = 0;
    this.received = 0;
    this.acked = 0;
    this.lastAckMs = 0;
  }
  /** Returns null when the chunk is in order, else an error code. */
  onData(seq, length) {
    if (seq !== this.nextSeq) return "seq-gap";
    if (this.received + length > this.size) return "size-mismatch";
    this.nextSeq += 1;
    this.received += length;
    return null;
  }
  ackDue(nowMs) {
    if (this.received === this.acked) return null;
    const due = this.received - this.acked >= FT_ACK_EVERY_BYTES ||
      nowMs - this.lastAckMs >= FT_ACK_EVERY_MS || this.received === this.size;
    if (!due) return null;
    this.acked = this.received;
    this.lastAckMs = nowMs;
    return this.received;
  }
  onDone(expected, actual) {
    if (this.received !== this.size) return "size-mismatch";
    if (expected.length !== actual.length || expected.some((byte, i) => byte !== actual[i])) return "hash-mismatch";
    return null;
  }
}

/** Sender-side flow control: at most FT_WINDOW_BYTES un-ACKed. */
export class FileSendWindow {
  constructor() { this.sent = 0; this.acked = 0; }
  canSend(length) { return this.sent - this.acked + length <= FT_WINDOW_BYTES; }
  onSent(length) { this.sent += length; }
  onAck(total) {
    if (total < this.acked || total > this.sent) return false;
    this.acked = total;
    return true;
  }
}

export function formatBytes(bytes) {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB"];
  let value = bytes / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) { value /= 1024; unit += 1; }
  return `${value >= 100 ? Math.round(value) : value.toFixed(1)} ${units[unit]}`;
}

const SHA256_K = new Uint32Array([
  0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
  0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
  0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
  0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
  0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
  0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
  0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
  0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
]);

/**
 * Incremental SHA-256 (WebCrypto only hashes whole buffers, and files can be
 * 2 GiB). `update(bytes)` any number of times, then `finish()` -> 32 bytes.
 */
export class Sha256 {
  constructor() {
    this.h = new Uint32Array([0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19]);
    this.w = new Uint32Array(64);
    this.block = new Uint8Array(64);
    this.blockLength = 0;
    this.bytes = 0;
  }

  update(data) {
    const bytes = bytesOf(data);
    let offset = 0;
    this.bytes += bytes.byteLength;
    if (this.blockLength > 0) {
      const take = Math.min(64 - this.blockLength, bytes.byteLength);
      this.block.set(bytes.subarray(0, take), this.blockLength);
      this.blockLength += take;
      offset = take;
      if (this.blockLength < 64) return this;
      this.compress(this.block, 0);
      this.blockLength = 0;
    }
    while (bytes.byteLength - offset >= 64) {
      this.compress(bytes, offset);
      offset += 64;
    }
    if (offset < bytes.byteLength) {
      this.block.set(bytes.subarray(offset), 0);
      this.blockLength = bytes.byteLength - offset;
    }
    return this;
  }

  finish() {
    const bitsHigh = Math.floor(this.bytes / 0x2000_0000);
    const bitsLow = (this.bytes * 8) >>> 0;
    const padding = new Uint8Array((this.blockLength < 56 ? 56 : 120) - this.blockLength + 8);
    padding[0] = 0x80;
    const view = new DataView(padding.buffer);
    view.setUint32(padding.length - 8, bitsHigh, false);
    view.setUint32(padding.length - 4, bitsLow, false);
    this.update(padding);
    const out = new Uint8Array(32);
    const outView = new DataView(out.buffer);
    for (let i = 0; i < 8; i += 1) outView.setUint32(i * 4, this.h[i], false);
    return out;
  }

  compress(bytes, offset) {
    const w = this.w;
    for (let i = 0; i < 16; i += 1) {
      const j = offset + i * 4;
      w[i] = (bytes[j] << 24) | (bytes[j + 1] << 16) | (bytes[j + 2] << 8) | bytes[j + 3];
    }
    for (let i = 16; i < 64; i += 1) {
      const a = w[i - 15];
      const b = w[i - 2];
      const s0 = ((a >>> 7) | (a << 25)) ^ ((a >>> 18) | (a << 14)) ^ (a >>> 3);
      const s1 = ((b >>> 17) | (b << 15)) ^ ((b >>> 19) | (b << 13)) ^ (b >>> 10);
      w[i] = (w[i - 16] + s0 + w[i - 7] + s1) | 0;
    }
    const h = this.h;
    let a = h[0], b = h[1], c = h[2], d = h[3], e = h[4], f = h[5], g = h[6], k = h[7];
    for (let i = 0; i < 64; i += 1) {
      const s1 = ((e >>> 6) | (e << 26)) ^ ((e >>> 11) | (e << 21)) ^ ((e >>> 25) | (e << 7));
      const ch = (e & f) ^ (~e & g);
      const t1 = (k + s1 + ch + SHA256_K[i] + w[i]) | 0;
      const s0 = ((a >>> 2) | (a << 30)) ^ ((a >>> 13) | (a << 19)) ^ ((a >>> 22) | (a << 10));
      const maj = (a & b) ^ (a & c) ^ (b & c);
      const t2 = (s0 + maj) | 0;
      k = g; g = f; f = e; e = (d + t1) | 0;
      d = c; c = b; b = a; a = (t1 + t2) | 0;
    }
    h[0] = (h[0] + a) | 0; h[1] = (h[1] + b) | 0; h[2] = (h[2] + c) | 0; h[3] = (h[3] + d) | 0;
    h[4] = (h[4] + e) | 0; h[5] = (h[5] + f) | 0; h[6] = (h[6] + g) | 0; h[7] = (h[7] + k) | 0;
  }
}
