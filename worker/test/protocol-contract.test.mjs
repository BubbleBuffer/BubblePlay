import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { validCurrentMediaEpoch } from "../src/protocol-validation.js";

const contract = JSON.parse(readFileSync(new URL("../../fixtures/wss-contract.json", import.meta.url), "utf8"));

test("session epochs must be canonical and match the current attachment", () => {
  for (const { value, accepted } of contract.epochs) {
    assert.equal(validCurrentMediaEpoch(value, { mediaEpoch: value }), accepted);
    assert.equal(validCurrentMediaEpoch(value, { mediaEpoch: "f".repeat(32) }), false);
  }
});
