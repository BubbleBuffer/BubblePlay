import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import {
  canonicalDeviceId,
  constantTimeEqual,
  decodeBase64url,
  deriveDeviceId,
  hashTicket,
  randomTicket,
  relayProofEncoding,
  relayProofHash,
  verifyRelayProof,
} from "../src/device-keys.js";

const CONTRACT = JSON.parse(readFileSync(new URL("../../fixtures/device-keys-contract.json", import.meta.url)));
const BASE64URL = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
const hexOf = (bytes) => Buffer.from(bytes).toString("hex");
const sha256Hex = async (bytes) => hexOf(await crypto.subtle.digest("SHA-256", bytes));

// The last base64url character of a 32-byte value carries two unused bits.
// Setting one of them keeps the length valid but makes the spelling non-canonical.
function withNonZeroPadBits(value) {
  const last = BASE64URL.indexOf(value.at(-1));
  return value.slice(0, -1) + BASE64URL[last | 1];
}

test("canonicalDeviceId returns every fixture ID in canonical form", () => {
  for (const entry of CONTRACT.ids) {
    assert.equal(canonicalDeviceId(entry.id), entry.id, entry.name);
    assert.equal(canonicalDeviceId(entry.id.replaceAll("-", "").toLowerCase()), entry.id, `${entry.name} lowercase`);
  }
});

test("canonicalDeviceId maps O, I and L and ignores hyphens and case", () => {
  const alias = CONTRACT.ids.find((entry) => entry.name === "crockford-aliases");
  assert.equal(canonicalDeviceId(alias.parse.input), alias.parse.canonical);
  assert.equal(canonicalDeviceId("hn0b-w2w6-0qq1-1m47"), "HN0B-W2W6-0QQ1-1M47");
  assert.equal(canonicalDeviceId("HNOB-W2W6-OQQI-LM47"), "HN0B-W2W6-0QQ1-1M47");
});

test("canonicalDeviceId refuses malformed input", () => {
  assert.equal(canonicalDeviceId("77VH-7M56-8GJK-Y12U"), null, "U is not in Crockford's alphabet");
  assert.equal(canonicalDeviceId("77VH-7M56-8GJK-Y12"), null, "15 characters");
  assert.equal(canonicalDeviceId("77VH-7M56-8GJK-Y12JA"), null, "17 characters");
  assert.equal(canonicalDeviceId("77VH 7M56 8GJK Y12J"), null, "spaces are not hyphens");
  assert.equal(canonicalDeviceId("77VH-7M56-8GJK-Y12!"), null, "punctuation");
  assert.equal(canonicalDeviceId(""), null);
  assert.equal(canonicalDeviceId(null), null);
  assert.equal(canonicalDeviceId(12345678901234567), null);
});

test("decodeBase64url round-trips canonical 32-byte values and rejects strict-format violations", () => {
  const publicKey = CONTRACT.ids[0].public;
  assert.equal(decodeBase64url(publicKey, 32).length, 32);
  assert.equal(decodeBase64url(publicKey).length, 32, "length is optional");
  assert.equal(decodeBase64url(publicKey, 31), null, "wrong length");
  assert.equal(decodeBase64url(`${publicKey}=`, 32), null, "padding");
  assert.equal(decodeBase64url(publicKey.replaceAll("-", "+").replaceAll("_", "/"), 32), null, "standard alphabet");
  assert.equal(decodeBase64url(withNonZeroPadBits(publicKey), 32), null, "non-zero unused bits");
  assert.equal(decodeBase64url(publicKey.slice(0, -1) + "*", 32), null, "invalid character");
  assert.equal(decodeBase64url(publicKey.slice(0, 41)), null, "41 characters cannot be base64url (remainder 1)");
  assert.equal(decodeBase64url(null, 32), null);
});

test("deriveDeviceId reproduces every fixture ID and its SHA-256", async () => {
  for (const entry of CONTRACT.ids) {
    assert.equal(await deriveDeviceId(entry.public), entry.id, entry.name);
    assert.equal(await sha256Hex(decodeBase64url(entry.public, 32)), entry.sha256Hex, `${entry.name} digest`);
  }
});

test("deriveDeviceId returns null for keys that are not 32 canonical bytes", async () => {
  const publicKey = CONTRACT.ids[0].public;
  assert.equal(await deriveDeviceId(publicKey.slice(0, 42)), null);
  assert.equal(await deriveDeviceId(`${publicKey}AAAA`), null, "35 bytes");
  assert.equal(await deriveDeviceId(withNonZeroPadBits(publicKey)), null);
  assert.equal(await deriveDeviceId(undefined), null);
});

test("randomTicket is 32 random bytes in unpadded base64url", () => {
  const first = randomTicket();
  const second = randomTicket();
  assert.match(first, /^[A-Za-z0-9_-]{43}$/);
  assert.equal(decodeBase64url(first, 32).length, 32);
  assert.notEqual(first, second);
});

test("hashTicket returns SHA-256 hex and refuses non-strings", async () => {
  assert.equal(await hashTicket("abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
  await assert.rejects(hashTicket(42));
});

test("constantTimeEqual compares equal-length strings and refuses the rest", async () => {
  const hash = await hashTicket("ticket");
  assert.equal(constantTimeEqual(hash, await hashTicket("ticket")), true);
  assert.equal(constantTimeEqual(hash, await hashTicket("ticket2")), false);
  assert.equal(constantTimeEqual(hash, hash.slice(1)), false, "different lengths");
  assert.equal(constantTimeEqual(hash, `${hash.slice(0, -1)}${hash.at(-1) === "0" ? "1" : "0"}`), false, "one character");
  assert.equal(constantTimeEqual(hash, undefined), false);
  assert.equal(constantTimeEqual(null, null), false);
});

for (const vector of CONTRACT.relayProof.vectors) {
  test(`relay proof ${vector.name} matches the fixture encoding and digest`, async () => {
    const input = { nonce: vector.nonce, id: vector.id, public: vector.public, origin: vector.origin, role: vector.role };
    assert.equal(hexOf(relayProofEncoding(input)), vector.encodingHex);
    assert.equal(hexOf(await relayProofHash(input)), vector.sha256Hex);
  });

  test(`relay proof ${vector.name} verifies as ${vector.expected}`, async () => {
    const valid = await verifyRelayProof({
      nonce: vector.nonce, id: vector.id, public: vector.public, origin: vector.origin,
      signature: vector.signature, role: vector.role,
    });
    assert.equal(valid, vector.expected === "valid");
  });
}

test("relay proof rejects altered inputs that the fixture vector would accept", async () => {
  const host = CONTRACT.relayProof.vectors.find((vector) => vector.name === "host");
  const base = { nonce: host.nonce, id: host.id, public: host.public, origin: host.origin, signature: host.signature };
  const viewerIdentity = CONTRACT.ids.find((entry) => entry.name === "viewer");
  assert.equal(await verifyRelayProof(base), true, "baseline");
  assert.equal(await verifyRelayProof({ ...base, role: "viewer" }), false, "role is part of the signed bytes");
  assert.equal(await verifyRelayProof({ ...base, origin: "https://example.test/" }), false, "origin spelling");
  assert.equal(await verifyRelayProof({ ...base, id: viewerIdentity.id }), false, "wrong ID for this key");
  assert.equal(await verifyRelayProof({ ...base, id: host.id.toLowerCase() }), false, "non-canonical ID is not signed bytes");
  assert.equal(await verifyRelayProof({ ...base, nonce: withNonZeroPadBits(host.nonce) }), false, "non-canonical nonce");
  assert.equal(await verifyRelayProof({ ...base, nonce: CONTRACT.relayProof.vectors[1].nonce }), false, "other nonce");
  assert.equal(await verifyRelayProof({ ...base, signature: host.signature.slice(0, -2) }), false, "short signature");
  assert.equal(await verifyRelayProof({ ...base, signature: undefined }), false, "missing signature");
  assert.equal(await verifyRelayProof({ ...base, public: viewerIdentity.public, id: host.id }), false, "public key not matching ID");
});

test("relay proof signature in the registration flow verifies against the host public key", async () => {
  const { registerChallenge, registerProof } = CONTRACT.relayProof.messages;
  const { hello } = CONTRACT.relayProof.messages;
  const host = CONTRACT.relayProof.vectors.find((vector) => vector.name === "host");
  assert.equal(registerProof.signature, host.signature);
  assert.equal(registerChallenge.nonce, host.nonce);
  assert.equal(await verifyRelayProof({
    nonce: registerChallenge.nonce, id: hello.id, public: hello.public, origin: host.origin, signature: registerProof.signature,
  }), true);
});

test("relay proof encoding refuses inputs it cannot encode canonically", async () => {
  const host = CONTRACT.relayProof.vectors.find((vector) => vector.name === "host");
  const input = { nonce: host.nonce, id: host.id, public: host.public, origin: host.origin };
  assert.equal(relayProofEncoding({ ...input, role: "relay" }), null, "unknown role");
  assert.equal(relayProofEncoding({ ...input, origin: undefined }), null, "origin must be a string");
  assert.equal(relayProofEncoding({ ...input, nonce: host.nonce.slice(0, -1) }), null, "short nonce");
  assert.equal(relayProofEncoding({ ...input, id: host.id.toLowerCase() }), null, "ID must be canonical");
  assert.equal(await relayProofHash({ ...input, public: "" }), null);
});
