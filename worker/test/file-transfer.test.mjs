import test from "node:test";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import {
  FT,
  FT_ACK_EVERY_BYTES,
  FT_ACK_EVERY_MS,
  FT_HEADER_BYTES,
  FT_IDLE_CLOSE_MS,
  FT_MAX_BATCH_FILES,
  FT_MAX_CONTROL_PAYLOAD_BYTES,
  FT_MAX_DATA_BYTES,
  FT_MAX_FILE_BYTES,
  FT_MAX_MESSAGE_BYTES,
  FT_MAX_NAME_BYTES,
  FT_TYPE_NAMES,
  FT_WINDOW_BYTES,
  FileIncoming,
  FileSendWindow,
  decodeFilePacket,
  encodeAccept,
  encodeAck,
  encodeCancel,
  encodeData,
  encodeDone,
  encodeOffer,
  encodeRequest,
  encodeResult,
  relayAcceptsFilePacket,
  validFileOffer,
} from "../public/file-transfer.js";

const contract = JSON.parse(readFileSync(new URL("../../fixtures/file-transfer-contract.json", import.meta.url), "utf8"));
const hex = value => Uint8Array.from(value.match(/../g) || [], byte => parseInt(byte, 16));
const toHex = bytes => Array.from(bytes, byte => byte.toString(16).padStart(2, "0")).join("");

function rejectedBytes(entry) {
  if (entry.hex !== undefined) return hex(entry.hex);
  const bytes = new Uint8Array(entry.totalBytes).fill(0xab);
  bytes.set(hex(entry.headerHex));
  return bytes;
}

function reencode(packet) {
  switch (packet.type) {
    case FT.OFFER: return encodeOffer(packet.transferId, packet.json);
    case FT.ACCEPT: return encodeAccept(packet.transferId);
    case FT.DATA: return encodeData(packet.transferId, packet.seq, packet.payload);
    case FT.ACK: return encodeAck(packet.transferId, packet.total);
    case FT.DONE: return encodeDone(packet.transferId, packet.sha256);
    case FT.CANCEL: return encodeCancel(packet.transferId, packet.json.reason);
    case FT.RESULT: return encodeResult(packet.transferId, packet.json.ok, packet.json.error);
    case FT.REQUEST: return encodeRequest(packet.transferId, packet.json.offerId, packet.json.id);
    default: throw new Error(`unknown type ${packet.type}`);
  }
}

test("file transfer limits and header layout match the shared fixture", () => {
  assert.equal(contract.magic, "BPF1");
  assert.equal(contract.headerBytes, FT_HEADER_BYTES);
  const limits = contract.limits;
  assert.equal(limits.maxDataBytes, FT_MAX_DATA_BYTES);
  assert.equal(limits.maxControlPayloadBytes, FT_MAX_CONTROL_PAYLOAD_BYTES);
  assert.equal(limits.maxMessageBytes, FT_MAX_MESSAGE_BYTES);
  assert.equal(limits.windowBytes, FT_WINDOW_BYTES);
  assert.equal(limits.ackEveryBytes, FT_ACK_EVERY_BYTES);
  assert.equal(limits.ackEveryMs, FT_ACK_EVERY_MS);
  assert.equal(limits.maxFileBytes, FT_MAX_FILE_BYTES);
  assert.equal(limits.maxBatchFiles, FT_MAX_BATCH_FILES);
  assert.equal(limits.maxNameBytes, FT_MAX_NAME_BYTES);
  assert.equal(limits.idleCloseMs, FT_IDLE_CLOSE_MS);
  for (const [name, value] of Object.entries(contract.types)) assert.equal(FT_TYPE_NAMES[value], name);
  assert.deepEqual(
    { type: contract.header.type, transferId: contract.header.transferId, seq: contract.header.seq },
    { type: 4, transferId: 8, seq: 12 });
});

test("file transfer packets decode and re-encode byte-exact (fixture from the Rust encoder)", () => {
  for (const entry of contract.packets) {
    const bytes = hex(entry.hex);
    const packet = decodeFilePacket(bytes);
    assert.equal(packet.ok, true, entry.name);
    assert.equal(packet.typeName, entry.type, entry.name);
    assert.equal(packet.transferId, entry.transferId, entry.name);
    assert.equal(packet.seq, entry.seq, entry.name);
    assert.equal(toHex(packet.payload), entry.payloadHex, entry.name);
    if (packet.json) assert.deepEqual(packet.json, entry.decoded, entry.name);
    if (packet.type === FT.ACK) assert.equal(packet.total, entry.decoded.total);
    if (packet.type === FT.DONE) assert.equal(toHex(packet.sha256), entry.decoded.sha256);
    if (packet.type === FT.DATA) assert.equal(toHex(packet.payload), entry.decoded.dataHex);
    assert.equal(toHex(reencode(packet)), entry.hex, `${entry.name} re-encodes`);
    assert.equal(relayAcceptsFilePacket(bytes.buffer, "file-client"), true, entry.name);
  }
});

test("file transfer rejects malformed packets with the fixture's reason, and the relay drops them", () => {
  for (const entry of contract.rejected) {
    const bytes = rejectedBytes(entry);
    const packet = decodeFilePacket(bytes);
    assert.equal(packet.ok, false, entry.name);
    assert.equal(packet.reason, entry.reason, entry.name);
    assert.equal(relayAcceptsFilePacket(bytes, "file-client"), false, entry.name);
    assert.equal(relayAcceptsFilePacket(bytes, "file-host"), false, entry.name);
  }
  // REQUEST only travels client -> host; text frames never.
  const request = contract.packets.find(entry => entry.type === "REQUEST");
  assert.equal(relayAcceptsFilePacket(hex(request.hex), "file-host"), false);
  assert.equal(relayAcceptsFilePacket("{}", "file-client"), false);
});

test("receiver sessions reach the fixture outcome (seq gap, size and hash mismatch)", () => {
  for (const session of contract.receiverSessions) {
    let incoming = null;
    let hash = createHash("sha256");
    let outcome = "ok";
    for (const step of session.packets) {
      const packet = decodeFilePacket(hex(step));
      assert.equal(packet.ok, true, session.name);
      let error = null;
      if (packet.type === FT.OFFER) incoming = new FileIncoming(packet.transferId, packet.json);
      else if (packet.type === FT.DATA) {
        error = incoming.onData(packet.seq, packet.payload.byteLength);
        if (!error) hash.update(packet.payload);
      } else if (packet.type === FT.DONE) {
        error = incoming.onDone(packet.sha256, new Uint8Array(hash.digest()));
        hash = createHash("sha256");
      }
      if (error) { outcome = error; break; }
    }
    assert.equal(outcome, session.outcome, session.name);
  }
});

test("file-offer signaling messages follow the fixture", () => {
  for (const entry of contract.signaling.fileOffer) {
    let parsed = null;
    try { parsed = JSON.parse(entry.text); } catch { /* invalid */ }
    assert.equal(validFileOffer(parsed), entry.accepted, entry.text.slice(0, 120));
  }
  assert.equal(JSON.parse(contract.signaling.hostCapabilities.text).fileTransfer, 1);
});

test("send window and ACK cadence", () => {
  const window = new FileSendWindow();
  let chunks = 0;
  while (window.canSend(FT_MAX_DATA_BYTES)) { window.onSent(FT_MAX_DATA_BYTES); chunks += 1; }
  assert.equal(chunks * FT_MAX_DATA_BYTES, FT_WINDOW_BYTES);
  assert.equal(window.onAck(FT_WINDOW_BYTES + 1), false);
  assert.equal(window.onAck(FT_ACK_EVERY_BYTES), true);
  assert.equal(window.onAck(FT_ACK_EVERY_BYTES - 1), false);
  assert.equal(window.canSend(FT_ACK_EVERY_BYTES), true);
  assert.equal(window.canSend(FT_ACK_EVERY_BYTES + 1), false);

  const incoming = new FileIncoming(1, { size: 600 * 1024 });
  const acks = [];
  let now = 1_000;
  for (let seq = 0; seq < 5; seq += 1) {
    assert.equal(incoming.onData(seq, FT_MAX_DATA_BYTES), null);
    const total = incoming.ackDue(now);
    if (total !== null) acks.push(total);
    now += 1;
  }
  assert.deepEqual(acks, [65_536, 5 * 65_536]);
  assert.equal(incoming.onData(5, FT_MAX_DATA_BYTES), null);
  assert.equal(incoming.ackDue(now), null);
  assert.equal(incoming.ackDue(now + FT_ACK_EVERY_MS), 6 * 65_536);
});
