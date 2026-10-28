// Browser viewer side of the device-key handshake: pairing, the control-channel AEAD and the
// DTLS bind. Byte encodings and rules: docs/architecture/device-keys.md ("Encodings for
// implementers" and "Phase 2 relay protocol"). WebCrypto only: no DOM, no sockets, no storage.
// Pair keys and control keys are non-extractable CryptoKeys unless a test asks for an export.
//
// Injection points marked TEST ONLY replay fixed inputs for the fixture vectors. Production
// callers never pass them.
import { canonicalId, deriveId } from "./devicekey.js";

const encoder = new TextEncoder();
const decoder = new TextDecoder("utf-8", { fatal: true });
const B64URL = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
const KIND = /^[a-z0-9-]{1,32}$/;
const CAP = /^[a-z0-9-]{1,64}$/;
const PAIR_ROLES = ["viewer", "pc"];
// RFC 8410 PKCS#8 wrapper for an X25519 private key; the 32-byte scalar follows. TEST ONLY.
const X25519_PKCS8_PREFIX = Uint8Array.from([0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x6e, 0x04, 0x22, 0x04, 0x20]);
const X25519_BASE_POINT = new Uint8Array(32);
X25519_BASE_POINT[0] = 9;

const subtle = () => globalThis.crypto.subtle;
const utf8 = value => encoder.encode(value);

/** A control message was refused. The channel that refused it is dead from then on. */
export class CtlError extends Error {
  constructor(message) {
    super(message);
    this.name = "CtlError";
  }
}

/** Unpadded base64url. `decode` is strict: padding, the standard alphabet and nonzero trailing bits are refused. */
export const b64u = {
  encode(bytes) {
    let text = "";
    let value = 0;
    let bits = 0;
    for (const byte of bytes) {
      value = (value << 8) | byte;
      bits += 8;
      while (bits >= 6) {
        bits -= 6;
        text += B64URL[(value >>> bits) & 63];
      }
      value &= (1 << bits) - 1;
    }
    if (bits > 0) text += B64URL[(value << (6 - bits)) & 63];
    return text;
  },
  decode(input) {
    if (typeof input !== "string" || !/^[A-Za-z0-9_-]*$/.test(input) || input.length % 4 === 1) {
      throw new RangeError("value is not unpadded base64url");
    }
    const bytes = new Uint8Array(Math.floor((input.length * 6) / 8));
    let value = 0;
    let bits = 0;
    let length = 0;
    for (const char of input) {
      value = (value << 6) | B64URL.indexOf(char);
      bits += 6;
      if (bits >= 8) {
        bits -= 8;
        bytes[length++] = (value >>> bits) & 0xff;
      }
      value &= (1 << bits) - 1;
    }
    if (value !== 0) throw new RangeError("value is not canonical base64url");
    return bytes;
  },
};

function expectBytes(value, length, name) {
  if (!(value instanceof Uint8Array) || value.length !== length) throw new TypeError(`${name} must be ${length} bytes`);
  return value;
}

function concat(...parts) {
  const out = new Uint8Array(parts.reduce((total, part) => total + part.length, 0));
  let offset = 0;
  for (const part of parts) {
    out.set(part, offset);
    offset += part.length;
  }
  return out;
}

// Compares every byte, with no early exit.
function bytesEqual(a, b) {
  if (a.length !== b.length) return false;
  let diff = 0;
  for (let i = 0; i < a.length; i++) diff |= a[i] ^ b[i];
  return diff === 0;
}

// uint32 big-endian byte length before each field.
function lengthPrefixed(fields) {
  const out = new Uint8Array(fields.reduce((total, field) => total + 4 + field.length, 0));
  const view = new DataView(out.buffer);
  let offset = 0;
  for (const field of fields) {
    view.setUint32(offset, field.length);
    out.set(field, offset + 4);
    offset += 4 + field.length;
  }
  return out;
}

async function digest(bytes) {
  return new Uint8Array(await subtle().digest("SHA-256", bytes));
}

// Verification fails closed: a malformed signature is a refusal, not an exception.
async function verifies(algorithm, key, signature, data) {
  try {
    return await subtle().verify(algorithm, key, signature, data);
  } catch {
    return false;
  }
}

function importEd25519Public(publicRaw) {
  return subtle().importKey("raw", publicRaw, { name: "Ed25519" }, false, ["verify"]);
}

function decodeOrNull(text, length) {
  try {
    const bytes = b64u.decode(text);
    return bytes.length === length ? bytes : null;
  } catch {
    return null;
  }
}

function isCanonicalId(value) {
  try {
    return canonicalId(value) === value;
  } catch {
    return false;
  }
}

function checkCaps(caps) {
  if (!Array.isArray(caps) || caps.length > 32 || !caps.every(cap => typeof cap === "string" && CAP.test(cap))) {
    throw new TypeError("caps must be at most 32 strings of 1-64 characters from [a-z0-9-]");
  }
}

function checkKind(kind) {
  if (typeof kind !== "string" || !KIND.test(kind)) throw new TypeError("kind must match [a-z0-9-]{1,32}");
}

// Per-direction nonce: four zero bytes, then the sequence number as uint64 big-endian.
function nonceFor(seq) {
  const nonce = new Uint8Array(12);
  new DataView(nonce.buffer).setBigUint64(4, BigInt(seq));
  return nonce;
}

/**
 * Pair code as typed or shown: dashes and spaces are dropped, case is ignored, O reads as 0 and
 * I or L as 1. Returns the 10 canonical characters or throws RangeError.
 * @param {string} input
 * @returns {string}
 */
export function normalizePairCode(input) {
  if (typeof input !== "string" || !/^[0-9A-Za-z\s-]*$/.test(input)) {
    throw new RangeError("a pair code is 10 Crockford base32 characters");
  }
  const code = input.replace(/[\s-]/g, "").toUpperCase().replaceAll("O", "0").replace(/[IL]/g, "1");
  if (!/^[0-9A-HJKMNP-TV-Z]{10}$/.test(code)) {
    throw new RangeError("a pair code is 10 Crockford base32 characters, such as 70Q1K-9MXR1");
  }
  return code;
}

/** Relay admit hash: b64url(SHA-256("bubbleplay-pair-admit-v1" || canonical PC ID || normalised code)). */
export async function pairAdmitHash(pcId, code) {
  const id = canonicalId(pcId);
  return b64u.encode(await digest(concat(utf8("bubbleplay-pair-admit-v1"), utf8(id), utf8(normalizePairCode(code)))));
}

/**
 * Handshake transcript H = SHA-256 of the length-prefixed fields. `epoch` is decimal text and is
 * never converted to a number. Public keys, nonces and ephemeral keys are raw 32-byte values.
 * @returns {Promise<{bytes: Uint8Array, hash: Uint8Array}>}
 */
export async function transcript({ relayOrigin, epoch, viewerId, viewerPublic, pcId, pcPublic, viewerNonce, pcNonce, viewerEphemeral, pcEphemeral }) {
  if (typeof relayOrigin !== "string" || !/^https?:\/\/[\x21-\x7e]+$/.test(relayOrigin)) {
    throw new TypeError("relayOrigin must be an http or https origin");
  }
  if (typeof epoch !== "string" || !/^[\x20-\x7e]{1,64}$/.test(epoch)) throw new TypeError("epoch must be 1-64 printable ASCII characters");
  if (!isCanonicalId(viewerId) || !isCanonicalId(pcId)) throw new TypeError("IDs must be canonical");
  const bytes = lengthPrefixed([
    utf8("bubbleplay-auth-v1"), utf8(relayOrigin), utf8(epoch), utf8(viewerId),
    expectBytes(viewerPublic, 32, "viewerPublic"), utf8(pcId), expectBytes(pcPublic, 32, "pcPublic"),
    expectBytes(viewerNonce, 32, "viewerNonce"), expectBytes(pcNonce, 32, "pcNonce"),
    expectBytes(viewerEphemeral, 32, "viewerEphemeral"), expectBytes(pcEphemeral, 32, "pcEphemeral"),
  ]);
  return { bytes, hash: await digest(bytes) };
}

// TEST ONLY: an X25519 key pair from a fixed scalar. The public key is the scalar times the base
// point (u = 9), which WebCrypto computes as an ordinary X25519 derivation.
async function x25519KeyPairFromSeed(seed) {
  const privateKey = await subtle().importKey("pkcs8", concat(X25519_PKCS8_PREFIX, seed), { name: "X25519" }, false, ["deriveBits"]);
  const basePoint = await subtle().importKey("raw", X25519_BASE_POINT, { name: "X25519" }, false, []);
  const publicBytes = new Uint8Array(await subtle().deriveBits({ name: "X25519", public: basePoint }, privateKey, 256));
  return { privateKey, publicKey: await subtle().importKey("raw", publicBytes, { name: "X25519" }, true, []) };
}

/**
 * Fresh viewer hello material: a 32-byte nonce and an X25519 ephemeral key pair whose private key
 * stays non-extractable. `caps` is checked and returned with the rest for buildAuthHello.
 * TEST ONLY: `nonce` and `ephemeralSeed` (32 bytes each) replace the random values.
 */
export async function newViewerHello({ caps, nonce, ephemeralSeed } = {}) {
  checkCaps(caps);
  const ephemeralKeyPair = ephemeralSeed === undefined
    ? await subtle().generateKey({ name: "X25519" }, false, ["deriveBits"])
    : await x25519KeyPairFromSeed(expectBytes(ephemeralSeed, 32, "ephemeralSeed"));
  return {
    caps,
    nonce: nonce === undefined ? globalThis.crypto.getRandomValues(new Uint8Array(32)) : expectBytes(nonce, 32, "nonce"),
    ephemeralKeyPair,
    ephemeralPublic: new Uint8Array(await subtle().exportKey("raw", ephemeralKeyPair.publicKey)),
  };
}

/** The auth-hello message with exactly the fixture's keys. Raw bytes in, base64url out. */
export function buildAuthHello({ id, publicRaw, nonce, ephemeralPublic, caps }) {
  if (!isCanonicalId(id)) throw new TypeError("id must be a canonical device ID");
  checkCaps(caps);
  return {
    type: "auth-hello",
    id,
    public: b64u.encode(expectBytes(publicRaw, 32, "publicRaw")),
    nonce: b64u.encode(expectBytes(nonce, 32, "nonce")),
    ephemeral: b64u.encode(expectBytes(ephemeralPublic, 32, "ephemeralPublic")),
    caps: [...caps],
  };
}

// Returns the auth-host fields in raw form, or null when the message is not exactly that shape.
function parseAuthHost(msg) {
  if (msg === null || typeof msg !== "object" || msg.type !== "auth-host" || !isCanonicalId(msg.id)) return null;
  if (Object.keys(msg).sort().join(",") !== "ephemeral,id,nonce,public,signature,type") return null;
  const publicRaw = decodeOrNull(msg.public, 32);
  const nonce = decodeOrNull(msg.nonce, 32);
  const ephemeral = decodeOrNull(msg.ephemeral, 32);
  const signature = decodeOrNull(msg.signature, 64);
  if (!publicRaw || !nonce || !ephemeral || !signature) return null;
  return { id: msg.id, publicRaw, nonce, ephemeral, signature };
}

/**
 * Checks the PC's auth-host. The PC's ID, public key, nonce and ephemeral key come from `msg`;
 * the viewer supplies its own side and the relay origin and epoch. This proves that `msg` came from
 * the key it names. The caller must check that `msg.id` is the PC it asked for.
 * Reasons: "bad-shape" (malformed, or the ID is not derived from the key), "pc-key-changed" (differs
 * from `pinnedPublic`), "bad-signature". `H` and `pcPublic` are returned only when ok.
 * @returns {Promise<{ok: boolean, reason: string|null, H: Uint8Array|null, pcPublic: Uint8Array|null}>}
 */
export async function verifyAuthHost({ msg, pinnedPublic = null, relayOrigin, epoch, viewerId, viewerPublic, viewerNonce, viewerEphemeral }) {
  const refused = reason => ({ ok: false, reason, H: null, pcPublic: null });
  const host = parseAuthHost(msg);
  if (!host || await deriveId(host.publicRaw) !== host.id) return refused("bad-shape");
  if (pinnedPublic !== null && !bytesEqual(expectBytes(pinnedPublic, 32, "pinnedPublic"), host.publicRaw)) {
    return refused("pc-key-changed");
  }
  const { hash } = await transcript({
    relayOrigin, epoch, viewerId, viewerPublic,
    pcId: host.id, pcPublic: host.publicRaw, viewerNonce, pcNonce: host.nonce,
    viewerEphemeral, pcEphemeral: host.ephemeral,
  });
  const key = await importEd25519Public(host.publicRaw);
  if (!await verifies({ name: "Ed25519" }, key, host.signature, concat(utf8("bubbleplay-auth-host-v1"), hash))) {
    return refused("bad-signature");
  }
  return { ok: true, reason: null, H: hash, pcPublic: host.publicRaw };
}

/**
 * The viewer's auth-finish signature over "bubbleplay-auth-viewer-v1" || H || the PC's raw signature.
 * Call it only after verifyAuthHost returned ok. `pcSignature` is the auth-host signature as sent.
 * @returns {Promise<string>} base64url signature
 */
export async function signAuthFinish(privateKey, hash, pcSignature) {
  const pcSigBytes = decodeOrNull(pcSignature, 64);
  if (pcSigBytes === null) throw new TypeError("pcSignature must be a 64-byte base64url signature");
  const signature = await subtle().sign({ name: "Ed25519" }, privateKey,
    concat(utf8("bubbleplay-auth-viewer-v1"), expectBytes(hash, 32, "H"), pcSigBytes));
  return b64u.encode(new Uint8Array(signature));
}

/**
 * Control keys from the X25519 exchange, salted with H. `peerEphemeralPublic` is the other side's raw
 * ephemeral key. `send` is viewer-to-pc and `recv` is pc-to-viewer whichever side calls, so a PC
 * channel swaps them. TEST ONLY: `extractable` lets a test compare raw bytes.
 * @returns {Promise<{send: CryptoKey, recv: CryptoKey}>}
 */
export async function deriveCtlKeys(ephemeralPrivateKey, peerEphemeralPublic, hash, { extractable = false } = {}) {
  const peer = await subtle().importKey("raw", expectBytes(peerEphemeralPublic, 32, "peerEphemeralPublic"), { name: "X25519" }, false, []);
  const shared = new Uint8Array(await subtle().deriveBits({ name: "X25519", public: peer }, ephemeralPrivateKey, 256));
  try {
    const base = await subtle().importKey("raw", shared, "HKDF", false, ["deriveKey"]);
    const derive = info => subtle().deriveKey(
      { name: "HKDF", hash: "SHA-256", salt: expectBytes(hash, 32, "H"), info: utf8(info) },
      base, { name: "AES-GCM", length: 256 }, extractable, ["encrypt", "decrypt"]);
    return {
      send: await derive("bubbleplay-ctl-v1 viewer-to-pc"),
      recv: await derive("bubbleplay-ctl-v1 pc-to-viewer"),
    };
  } finally {
    shared.fill(0);
  }
}

// Reads an incoming envelope. Anything malformed throws CtlError (the caller ends the channel).
function readEnvelope(msg) {
  if (msg === null || typeof msg !== "object" || msg.type !== "ctl") throw new CtlError("control message has the wrong shape");
  if (Object.keys(msg).sort().join(",") !== "data,kind,seq,type") throw new CtlError("control message has the wrong shape");
  if (typeof msg.kind !== "string" || !KIND.test(msg.kind)) throw new CtlError("control kind must match [a-z0-9-]{1,32}");
  if (!Number.isSafeInteger(msg.seq)) throw new CtlError("control sequence must be an integer");
  let data;
  try {
    data = b64u.decode(msg.data);
  } catch {
    throw new CtlError("control data is not base64url");
  }
  return { kind: msg.kind, seq: msg.seq, data };
}

/**
 * One control channel: `sendKey` seals this side's messages, `recvKey` opens the other side's.
 * Sequence numbers start at 0 per direction. A refused message ends the channel, and every later
 * call rejects with CtlError. A refusal is a gap, a repeat, a tampered or wrong-kind message, or a
 * JSON payload whose `type` is not the kind. Calls run one at a time, in call order, so sealed
 * messages come out in sequence and opened ones are checked in sequence.
 * - seal(kind, bytes) -> {type, kind, seq, data}
 * - sealJson(obj) -> seal(obj.type, JSON bytes)
 * - open(msg) -> plaintext bytes. Use for binary payloads (input frames).
 * - openJson(msg) -> parsed object. Its `type` must equal the message kind.
 * @returns {{seal: Function, sealJson: Function, open: Function, openJson: Function}}
 */
export function createCtlChannel({ sendKey, recvKey }) {
  let sendSeq = 0;
  let recvSeq = 0;
  let ended = false;
  let queue = Promise.resolve();
  const inOrder = task => {
    const run = queue.then(task);
    queue = run.catch(() => {});
    return run;
  };

  async function sealNow(kind, plaintext) {
    if (ended) throw new CtlError("control channel has ended");
    checkKind(kind);
    if (!(plaintext instanceof Uint8Array)) throw new TypeError("plaintext must be a Uint8Array");
    const seq = sendSeq;
    let sealed;
    try {
      sealed = await subtle().encrypt({ name: "AES-GCM", iv: nonceFor(seq), additionalData: utf8(kind), tagLength: 128 }, sendKey, plaintext);
    } catch {
      ended = true;
      throw new CtlError("control message could not be sealed");
    }
    sendSeq += 1;
    return { type: "ctl", kind, seq, data: b64u.encode(new Uint8Array(sealed)) };
  }

  async function openNow(msg) {
    if (ended) throw new CtlError("control channel has ended");
    try {
      const { kind, seq, data } = readEnvelope(msg);
      if (seq !== recvSeq) throw new CtlError(`control sequence ${seq} arrived where ${recvSeq} was expected`);
      let plaintext;
      try {
        plaintext = await subtle().decrypt({ name: "AES-GCM", iv: nonceFor(seq), additionalData: utf8(kind), tagLength: 128 }, recvKey, data);
      } catch {
        throw new CtlError("control message failed authentication");
      }
      recvSeq += 1;
      return new Uint8Array(plaintext);
    } catch (error) {
      ended = true;
      throw error;
    }
  }

  async function openJsonNow(msg) {
    const plaintext = await openNow(msg);
    let value;
    try {
      value = JSON.parse(decoder.decode(plaintext));
    } catch {
      ended = true;
      throw new CtlError("control payload is not JSON");
    }
    if (value === null || typeof value !== "object" || value.type !== msg.kind) {
      ended = true;
      throw new CtlError("control payload type does not match its kind");
    }
    return value;
  }

  return {
    seal: (kind, plaintext) => inOrder(() => sealNow(kind, plaintext)),
    sealJson: obj => inOrder(() => sealNow(obj?.type, utf8(JSON.stringify(obj)))),
    open: msg => inOrder(() => openNow(msg)),
    openJson: msg => inOrder(() => openJsonNow(msg)),
  };
}

/**
 * Pair key K = HKDF-SHA256(ikm = normalised code, salt = empty, info "bubbleplay-pair-v1"), as an
 * HMAC key. TEST ONLY: `extractable` lets a test compare raw bytes.
 * @returns {Promise<CryptoKey>}
 */
export async function pairKey(code, { extractable = false } = {}) {
  const base = await subtle().importKey("raw", utf8(normalizePairCode(code)), "HKDF", false, ["deriveKey"]);
  return subtle().deriveKey(
    { name: "HKDF", hash: "SHA-256", salt: new Uint8Array(0), info: utf8("bubbleplay-pair-v1") },
    base, { name: "HMAC", hash: "SHA-256", length: 256 }, extractable, ["sign", "verify"]);
}

/** HMAC-SHA256(K, role || H) with role "viewer" or "pc". Returns base64url. */
export async function pairMac(key, role, hash) {
  if (!PAIR_ROLES.includes(role)) throw new TypeError('role must be "viewer" or "pc"');
  const mac = await subtle().sign("HMAC", key, concat(utf8(role), expectBytes(hash, 32, "H")));
  return b64u.encode(new Uint8Array(mac));
}

/** Recomputes the MAC for `role` and compares it in constant time. Malformed input is false. */
export async function verifyPairMac(key, role, hash, macText) {
  const expected = b64u.decode(await pairMac(key, role, hash));
  const received = decodeOrNull(macText, 32);
  return received !== null && bytesEqual(expected, received);
}

/** Six-digit comparison number: first 4 bytes of SHA-256("bubbleplay-pair-sas-v1" || H), big-endian, mod 10^6. */
export async function comparisonNumber(hash) {
  const digestBytes = await digest(concat(utf8("bubbleplay-pair-sas-v1"), expectBytes(hash, 32, "H")));
  return String(new DataView(digestBytes.buffer).getUint32(0) % 1000000).padStart(6, "0");
}

// Each signer's own local and remote fingerprints, normalised (decision 23), joined by one LF byte.
export function bindMessage(hash, localFingerprint, remoteFingerprint) {
  const local = normalizeFingerprint(localFingerprint);
  const remote = normalizeFingerprint(remoteFingerprint);
  return concat(utf8("bubbleplay-dtls-v1"), expectBytes(hash, 32, "H"), utf8(`${local}\n${remote}`));
}

/** DTLS bind signature. Pass the signer's own local and remote fingerprints. Returns base64url. */
export async function signBind(privateKey, hash, localFingerprint, remoteFingerprint) {
  const signature = await subtle().sign({ name: "Ed25519" }, privateKey, bindMessage(hash, localFingerprint, remoteFingerprint));
  return b64u.encode(new Uint8Array(signature));
}

/**
 * Checks a DTLS bind. Pass the SIGNER's local and remote fingerprints in the signer's order. To
 * check the PC's bind, pass the PC's local (the viewer's remote) first.
 * @returns {Promise<boolean>}
 */
export async function verifyBind(publicRaw, hash, localFingerprint, remoteFingerprint, signature) {
  const message = bindMessage(hash, localFingerprint, remoteFingerprint);
  const sig = decodeOrNull(signature, 64);
  if (sig === null) return false;
  return verifies({ name: "Ed25519" }, await importEd25519Public(expectBytes(publicRaw, 32, "publicRaw")), sig, message);
}

/**
 * Decision 23 form of a DTLS fingerprint: "sha-256", one space, uppercase hex pairs joined by
 * colons. Accepts any letter case and spaces or tabs between the two parts. Only sha-256.
 * @returns {string}
 */
export function normalizeFingerprint(value) {
  if (typeof value !== "string") throw new TypeError("fingerprint must be a string");
  const match = /^([A-Za-z0-9-]+)[ \t]+((?:[0-9A-Fa-f]{2}:){31}[0-9A-Fa-f]{2})$/.exec(value);
  if (!match || match[1].toLowerCase() !== "sha-256") {
    throw new TypeError("fingerprint must be sha-256 with 32 colon-separated hex bytes");
  }
  return `sha-256 ${match[2].toUpperCase()}`;
}

/**
 * The normalised sha-256 `a=fingerprint` value of an SDP (decision 23). Other algorithms are
 * skipped. Returns null when the SDP has no sha-256 fingerprint; throws on a malformed one or
 * on several distinct ones. Repeats of the same value are fine.
 * @returns {string|null}
 */
export function parseSdpFingerprint(sdp) {
  if (typeof sdp !== "string") throw new TypeError("sdp must be a string");
  const values = new Set();
  for (const line of sdp.split(/\r?\n/)) {
    if (!line.startsWith("a=fingerprint:")) continue;
    const value = line.slice("a=fingerprint:".length);
    if (/^[A-Za-z0-9-]+/.exec(value)?.[0].toLowerCase() !== "sha-256") continue;
    try { values.add(normalizeFingerprint(value)); } catch { throw new RangeError("a=fingerprint sha-256 value is malformed"); }
  }
  if (values.size > 1) throw new RangeError(`SDP carries ${values.size} distinct sha-256 fingerprints`);
  return values.size ? [...values][0] : null;
}
