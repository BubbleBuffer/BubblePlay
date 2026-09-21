import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { validCurrentMediaEpoch, validWssTopology } from "../src/protocol-validation.js";
import { CODEC, shardFrameV2, validateWssTopology, decodeMediaShardV2 } from "../public/media.js";

const contract = JSON.parse(readFileSync(new URL("../../fixtures/wss-contract.json", import.meta.url), "utf8"));
test("browser, relay, and host share topology admission and v2 wire bytes", () => {
  for (const { accepted, ...topology } of contract.topologies) {
    assert.equal(validWssTopology(topology), accepted, JSON.stringify(topology));
    assert.equal(Boolean(validateWssTopology(topology)), accepted);
  }
  const { data, packets, ...metadata } = contract.v2;
  const actual = shardFrameV2({ ...metadata, codec: CODEC.H264, key: true, data: Uint8Array.from(data) });
  assert.deepEqual(actual.map(packet => Buffer.from(packet).toString("hex")), packets);
  for (const hex of packets) {
    const packet = Uint8Array.from(Buffer.from(hex, "hex"));
    assert.ok(decodeMediaShardV2(packet));
    assert.equal(decodeMediaShardV2(packet.slice(0, 23)), null);
  }
});

test("session epochs must be canonical and match the current attachment", () => {
  for (const { value, accepted } of contract.epochs) {
    assert.equal(validCurrentMediaEpoch(value, { mediaEpoch: value }), accepted);
    assert.equal(validCurrentMediaEpoch(value, { mediaEpoch: "f".repeat(32) }), false);
  }
});
