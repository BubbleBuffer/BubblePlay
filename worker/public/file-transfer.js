// File transfer protocol v1, shared by the relay (worker/src) and the web
// receiver. Intent and wire contract: .superpawers/file-transfer.md. Shared
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

export function validOfferMeta(meta) {
  return plainObject(meta) &&
    typeof meta.name === "string" && meta.name.length > 0 && utf8Bytes(meta.name) <= FT_MAX_NAME_BYTES &&
    Number.isSafeInteger(meta.size) && meta.size >= 0 && meta.size <= FT_MAX_FILE_BYTES &&
    typeof meta.mime === "string" && utf8Bytes(meta.mime) <= FT_MAX_MIME_BYTES &&
    Number.isSafeInteger(meta.mtimeMs) && meta.mtimeMs >= 0 &&
    (meta.purpose === "save" || meta.purpose === "paste") &&
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
