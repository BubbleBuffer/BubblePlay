// Device-key fixtures for the phase-1 routing tests. Everything here is a public,
// deterministic TEST vector from fixtures/device-keys-contract.json; never use these
// seeds for a real device. The signer is written independently from src/device-keys.js
// (WebCrypto Ed25519 over the encoding in docs/architecture/device-keys.md), so the
// relay's verifier is checked against a second implementation, not against itself.
import { readFileSync } from "node:fs";

export const contract = JSON.parse(readFileSync(new URL("../../../fixtures/device-keys-contract.json", import.meta.url)));

const entry = name => contract.ids.find(item => item.name === name);
export const PC = Object.freeze(entry("pc"));
export const VIEWER = Object.freeze(entry("viewer"));
export const ALIAS_PC = Object.freeze(entry("crockford-aliases"));
export const RELAY_ORIGIN = "https://play.example";

export const b64url = bytes => Buffer.from(bytes).toString("base64url");
export const unb64url = value => new Uint8Array(Buffer.from(value, "base64url"));
const hexBytes = hex => new Uint8Array(Buffer.from(hex, "hex"));

// Random 32-byte nonce, as the relay issues in register-challenge.
export function freshNonce() { return b64url(crypto.getRandomValues(new Uint8Array(32))); }

// Flips one bit of a base64url signature. The result is still canonical base64url,
// so it fails on the signature check rather than on decoding.
export function tamper(signature) {
  const bytes = unb64url(signature);
  bytes[0] ^= 0x01;
  return b64url(bytes);
}

const PKCS8_ED25519_PREFIX = hexBytes("302e020100300506032b657004220420");
async function privateKey(seedHex) {
  const pkcs8 = new Uint8Array([...PKCS8_ED25519_PREFIX, ...hexBytes(seedHex)]);
  return crypto.subtle.importKey("pkcs8", pkcs8, { name: "Ed25519" }, false, ["sign"]);
}

// uint32 big-endian length || bytes, for each part. Strings are UTF-8.
function lengthPrefixed(parts) {
  const chunks = [];
  for (const part of parts) {
    const bytes = typeof part === "string" ? new TextEncoder().encode(part) : part;
    const head = new Uint8Array(4);
    new DataView(head.buffer).setUint32(0, bytes.length);
    chunks.push(head, bytes);
  }
  const out = new Uint8Array(chunks.reduce((sum, chunk) => sum + chunk.length, 0));
  let offset = 0;
  for (const chunk of chunks) { out.set(chunk, offset); offset += chunk.length; }
  return out;
}

export function relayProofEncoding({ role = "host", origin, nonce, id, public: publicKey }) {
  return lengthPrefixed(["bubbleplay-relay-v1", role, origin, unb64url(nonce), id, unb64url(publicKey)]);
}

// Signs SHA-256(encoding) with the seed. Ed25519 signs those 32 bytes directly.
export async function signRelayProof({ seedHex, role = "host", origin, nonce, id, public: publicKey }) {
  const digest = new Uint8Array(await crypto.subtle.digest("SHA-256",
    relayProofEncoding({ role, origin, nonce, id, public: publicKey })));
  const signature = await crypto.subtle.sign({ name: "Ed25519" }, await privateKey(seedHex), digest);
  return b64url(new Uint8Array(signature));
}

export async function verifyWithFixtureKey({ role = "host", origin, nonce, id, public: publicKey, signature }) {
  const digest = new Uint8Array(await crypto.subtle.digest("SHA-256",
    relayProofEncoding({ role, origin, nonce, id, public: publicKey })));
  const key = await crypto.subtle.importKey("raw", unb64url(publicKey), { name: "Ed25519" }, false, ["verify"]);
  return crypto.subtle.verify({ name: "Ed25519" }, key, unb64url(signature), digest);
}
