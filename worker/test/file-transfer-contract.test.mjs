import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import * as ft from "../public/file-transfer.js";

const contract = JSON.parse(readFileSync(new URL("../../fixtures/file-transfer-contract.json", import.meta.url), "utf8"));
const bytes = (hex) => Uint8Array.from(Buffer.from(hex, "hex"));
const hex = (view) => Buffer.from(view).toString("hex");

test("browser file-transfer constants equal the contract", () => {
  const { limits } = contract;
  assert.deepEqual([...ft.FT_MAGIC], [...Buffer.from(contract.magic, "latin1")]);
  assert.equal(ft.FT_HEADER_BYTES, contract.headerBytes);
  assert.deepEqual({ ...ft.FT }, contract.types);
  assert.equal(ft.FT_MAX_DATA_BYTES, limits.maxDataBytes);
  assert.equal(ft.FT_MAX_CONTROL_PAYLOAD_BYTES, limits.maxControlPayloadBytes);
  assert.equal(ft.FT_MAX_MESSAGE_BYTES, limits.maxMessageBytes);
  assert.equal(ft.FT_WINDOW_BYTES, limits.windowBytes);
  assert.equal(ft.FT_ACK_EVERY_BYTES, limits.ackEveryBytes);
  assert.equal(ft.FT_ACK_EVERY_MS, limits.ackEveryMs);
  assert.equal(ft.FT_MAX_FILE_BYTES, limits.maxFileBytes);
  assert.equal(ft.FT_MAX_BATCH_FILES, limits.maxBatchFiles);
  assert.equal(ft.FT_MAX_NAME_BYTES, limits.maxNameBytes);
  assert.equal(ft.FT_IDLE_CLOSE_MS, limits.idleCloseMs);
});

test("every contract packet decodes to its fields and re-encodes to the same bytes", () => {
  for (const packet of contract.packets) {
    const decoded = ft.decodeFilePacket(bytes(packet.hex));
    assert.equal(decoded.ok, true, packet.name);
    assert.equal(decoded.typeName, packet.type, packet.name);
    assert.equal(decoded.transferId, packet.transferId, packet.name);
    assert.equal(decoded.seq, packet.seq, packet.name);
    assert.equal(hex(decoded.payload), packet.payloadHex, packet.name);
    assert.equal(hex(ft.encodeFilePacket(decoded.type, decoded.transferId, decoded.seq, decoded.payload)), packet.hex, packet.name);
    const want = packet.decoded;
    if (want?.total !== undefined) assert.equal(decoded.total, want.total, packet.name);
    if (want?.sha256 !== undefined) assert.equal(hex(decoded.sha256), want.sha256, packet.name);
    if (want?.dataHex !== undefined) assert.equal(hex(decoded.payload), want.dataHex, packet.name);
    if (decoded.json) assert.deepEqual(decoded.json, want, packet.name);
  }
});

test("every rejected contract packet fails with the contract's reason", () => {
  for (const bad of contract.rejected) {
    const message = bad.hex
      ? bytes(bad.hex)
      : Uint8Array.from([...bytes(bad.headerHex), ...new Uint8Array(bad.totalBytes - bytes(bad.headerHex).length).fill(0xab)]);
    assert.deepEqual(ft.decodeFilePacket(message), { ok: false, reason: bad.reason }, bad.name);
  }
});
