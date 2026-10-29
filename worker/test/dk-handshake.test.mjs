import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { beginViewerHandshake } from "../public/dk-handshake.js";

const { handshake: hs } = JSON.parse(readFileSync(new URL("../../fixtures/device-keys-contract.json", import.meta.url)));
const bytes = value => new Uint8Array(Buffer.from(value, "base64url"));
const hex = value => new Uint8Array(Buffer.from(value, "hex"));

async function setup(pinnedPublic = bytes(hs.pc.public), pcId = hs.pc.id) {
  const privateKey = await crypto.subtle.importKey("pkcs8", hex("302e020100300506032b657004220420" + hs.viewer.seedHex), "Ed25519", false, ["sign"]);
  const sent = [];
  const handshake = await beginViewerHandshake({ identity: { id: hs.viewer.id, publicRaw: bytes(hs.viewer.public), privateKey },
    pcId, pinnedPublic, relayOrigin: hs.relayOrigin, epoch: hs.sessionEpoch,
    helloOptions: { nonce: bytes(hs.viewer.nonce), ephemeralSeed: hex(hs.viewer.ephemeralPrivateHex) }, send: msg => sent.push(msg) });
  return { handshake, sent };
}

test("fixture auth-hello and auth-finish are exact", async () => {
  const { handshake, sent } = await setup();
  assert.deepEqual(sent, [hs.messages.authHello]);
  const result = await handshake.handleAuthHost(hs.messages.authHost);
  assert.deepEqual(sent[1], hs.messages.authFinish);
  assert.deepEqual(result.pcPublic, bytes(hs.pc.public));
  assert.deepEqual(result.H, hex(hs.transcriptHashHex));
});

test("changed key, wrong target and bad signature never send auth-finish", async () => {
  for (const [pin, target, host, reason] of [
    [bytes(hs.viewer.public), hs.pc.id, hs.messages.authHost, "pc-key-changed"],
    [bytes(hs.pc.public), hs.pc.id, { ...hs.messages.authHost, public: hs.viewer.public }, "pc-key-changed"],
    [bytes(hs.pc.public), hs.viewer.id, hs.messages.authHost, "pc-key-changed"],
    [bytes(hs.pc.public), hs.pc.id, { ...hs.messages.authHost, signature: hs.messages.authFinish.signature }, "bad-signature"],
  ]) {
    const { handshake, sent } = await setup(pin, target);
    await assert.rejects(handshake.handleAuthHost(host), error => error.reason === reason);
    assert.equal(sent.length, 1);
  }
});
