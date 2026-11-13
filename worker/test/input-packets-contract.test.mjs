import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { TEXT_CHUNK_BYTES } from "../public/keyboard-draft.js";
import { packet } from "../public/input.js";

// fixtures/input-contract.json "pointerKeyTextPackets": the browser's encoders against the shared vectors.
const contract = JSON.parse(readFileSync(new URL("../../fixtures/input-contract.json", import.meta.url), "utf8")).pointerKeyTextPackets;
const hex = (bytes) => Buffer.from(bytes).toString("hex");

const ENCODERS = {
  key: (d) => packet.key(d.vk, d.up),
  buttons: (d) => packet.buttons(d.mask),
  releaseAll: () => packet.releaseAll(),
  motion: (d) => packet.motion(d.seq, d.dx, d.dy, d.wheelX, d.wheelY),
  absolute: (d) => packet.absolute(d.seq, d.x, d.y),
  buttonsAt: (d) => packet.buttonsAt(d.seq, d.x, d.y, d.mask),
  cursorSync: (d) => packet.cursorSync(d.seq),
  text: (d) => packet.text(d.text),
};

test("browser input encoders produce the contract bytes", () => {
  for (const vector of contract.vectors) {
    assert.equal(hex(ENCODERS[vector.decoded.kind](vector.decoded)), vector.hex, vector.name);
  }
});

test("browser text packets stop at the contract limit", () => {
  assert.ok(packet.text("x".repeat(contract.textMaxBytes)));
  assert.equal(packet.text("x".repeat(contract.textMaxBytes + 1)), null);
  assert.equal(TEXT_CHUNK_BYTES, contract.textChunkBytes);
  assert.ok(TEXT_CHUNK_BYTES <= contract.textMaxBytes);
  assert.equal(packet.text(""), null);
});
