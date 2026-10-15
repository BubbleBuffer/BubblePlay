// Device IDs, relay proofs and tickets for phase 1 routing. Encodings are
// fixed by docs/architecture/device-keys.md ("Encodings for implementers") and
// fixtures/device-keys-contract.json. No dependencies: WebCrypto only.

const CROCKFORD = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const CANONICAL_BODY = /^[0-9A-HJKMNP-TV-Z]{16}$/;
const RELAY_CONTEXT = "bubbleplay-relay-v1";
const ROLES = new Set(["host", "viewer"]);
const encoder = new TextEncoder();

// Canonical form XXXX-XXXX-XXXX-XXXX. Hyphens and case are ignored; O reads as
// 0 and I or L as 1. U is not in Crockford's alphabet, so it is refused.
export function canonicalDeviceId(input) {
  if (typeof input !== "string") return null;
  const body = input.replace(/-/g, "").toUpperCase().replace(/O/g, "0").replace(/[IL]/g, "1");
  return CANONICAL_BODY.test(body) ? groupId(body) : null;
}

// Strict unpadded base64url. Rejects padding, the standard alphabet, bad
// lengths and non-zero unused bits, so each byte string has one spelling.
export function decodeBase64url(value, length) {
  if (typeof value !== "string" || !/^[A-Za-z0-9_-]*$/.test(value) || value.length % 4 === 1) return null;
  const padded = value.replace(/-/g, "+").replace(/_/g, "/") + "=".repeat((4 - value.length % 4) % 4);
  let binary;
  try { binary = atob(padded); } catch { return null; }
  const bytes = Uint8Array.from(binary, (char) => char.charCodeAt(0));
  if (length !== undefined && bytes.length !== length) return null;
  return encodeBase64url(bytes) === value ? bytes : null;
}

// SHA-256 of the raw 32-byte Ed25519 public key, first 80 bits, base32.
export async function deriveDeviceId(publicString) {
  const publicKey = decodeBase64url(publicString, 32);
  if (!publicKey) return null;
  const digest = new Uint8Array(await crypto.subtle.digest("SHA-256", publicKey));
  return groupId(base32(digest.subarray(0, 10)));
}

export function randomTicket() {
  return encodeBase64url(crypto.getRandomValues(new Uint8Array(32)));
}

export async function hashTicket(token) {
  if (typeof token !== "string") throw new TypeError("token must be a string");
  return hex(await crypto.subtle.digest("SHA-256", encoder.encode(token)));
}

// Compares equal-length strings in time that depends only on their length.
// Unequal lengths return false at once, which leaks nothing for hash strings.
export function constantTimeEqual(a, b) {
  if (typeof a !== "string" || typeof b !== "string" || a.length !== b.length) return false;
  let difference = 0;
  for (let index = 0; index < a.length; index += 1) difference |= a.charCodeAt(index) ^ b.charCodeAt(index);
  return difference === 0;
}

// Exact bytes that get hashed. Each field is uint32 big-endian length || bytes.
// The role is "host" for the PC and "viewer" for a viewer. Returns null for
// input that cannot be encoded canonically.
export function relayProofEncoding({ nonce, id, public: publicString, origin, role = "host" }) {
  if (!ROLES.has(role) || typeof origin !== "string") return null;
  const nonceBytes = decodeBase64url(nonce, 32);
  const publicKey = decodeBase64url(publicString, 32);
  if (!nonceBytes || !publicKey || canonicalDeviceId(id) !== id) return null;
  return lengthPrefixed(
    encoder.encode(RELAY_CONTEXT),
    encoder.encode(role),
    encoder.encode(origin),
    nonceBytes,
    encoder.encode(id),
    publicKey,
  );
}

// SHA-256 of the relay proof encoding. The signature covers these 32 bytes.
export async function relayProofHash(input) {
  const encoding = relayProofEncoding(input);
  if (!encoding) return null;
  return new Uint8Array(await crypto.subtle.digest("SHA-256", encoding));
}

// True only when the Ed25519 signature checks out for the claimed ID, key,
// nonce and origin. Any malformed input or WebCrypto error returns false.
export async function verifyRelayProof({ nonce, id, public: publicString, origin, signature, role = "host" }) {
  try {
    const publicKey = decodeBase64url(publicString, 32);
    const signatureBytes = decodeBase64url(signature, 64);
    if (!publicKey || !signatureBytes) return false;
    if ((await deriveDeviceId(publicString)) !== id) return false;
    const hash = await relayProofHash({ nonce, id, public: publicString, origin, role });
    if (!hash) return false;
    const key = await crypto.subtle.importKey("raw", publicKey, { name: "Ed25519" }, false, ["verify"]);
    return await crypto.subtle.verify({ name: "Ed25519" }, key, signatureBytes, hash);
  } catch {
    return false;
  }
}

function encodeBase64url(bytes) {
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

function base32(bytes) {
  let bits = 0;
  let value = 0;
  let out = "";
  for (const byte of bytes) {
    value = ((value << 8) | byte) & 0xffff;
    bits += 8;
    while (bits >= 5) {
      out += CROCKFORD[(value >>> (bits - 5)) & 31];
      bits -= 5;
    }
  }
  // Ten bytes is exactly 16 characters, so no bits are left over. Other
  // lengths would need padding, which no caller uses.
  if (bits > 0) out += CROCKFORD[(value << (5 - bits)) & 31];
  return out;
}

function groupId(body) {
  return `${body.slice(0, 4)}-${body.slice(4, 8)}-${body.slice(8, 12)}-${body.slice(12, 16)}`;
}

function hex(buffer) {
  return [...new Uint8Array(buffer)].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}

function lengthPrefixed(...parts) {
  const total = parts.reduce((sum, part) => sum + 4 + part.length, 0);
  const out = new Uint8Array(total);
  const view = new DataView(out.buffer);
  let offset = 0;
  for (const part of parts) {
    view.setUint32(offset, part.length);
    offset += 4;
    out.set(part, offset);
    offset += part.length;
  }
  return out;
}
