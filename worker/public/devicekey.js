// Device keys: IDs, `.bpkey` key files, unlock and rewrap. No UI and no storage here.
// Format and rules: docs/architecture/device-keys.md (Identity, Phase 0 encodings).
//
// Secrets: seeds, PKCS#8 copies, UTF-8 passphrases and derived keys are byte buffers
// we own. They are overwritten in `finally`, including on error paths. JavaScript strings
// (the JSON text of a key file, a passphrase, the JWK `d`) cannot be wiped; callers should
// drop their references after use. Nothing is written to storage, sessionStorage or localStorage.
import { deriveKeyInWorker } from "./devicekey-kdf-worker.js";

const FORMAT = "bubbleplay-device-key";
const VERSION = 1;
const TYPE = "ed25519";
const KDF_ALG = "argon2id";
const AEAD_ALG = "aes-256-gcm";
// Fixed by the spec. The worker hard-codes the same Argon2id parameters.
const KDF_PARAMS = Object.freeze({ m: 65536, t: 3, p: 1 });
const SALT_BYTES = 16;
const NONCE_BYTES = 12;
const SEED_BYTES = 32;
const PUBLIC_BYTES = 32;
const KEY_BYTES = 32;
const TAG_BYTES = 16;
// RFC 8410 PKCS#8 wrapper for an Ed25519 private key; the 32-byte seed follows.
const PKCS8_ED25519_PREFIX = Uint8Array.from([0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20]);
// Crockford base32: no I, L, O or U.
const CROCKFORD = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const CANONICAL_ID = /^[0-9A-HJKMNP-TV-Z]{4}(?:-[0-9A-HJKMNP-TV-Z]{4}){3}$/;
const B64URL = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
// RFC 3339 date-time. Fractions and offsets are allowed; lowercase t and z are accepted too.
const RFC3339 = /^(\d{4})-(\d{2})-(\d{2})[Tt](\d{2}):(\d{2}):(\d{2})(?:\.\d+)?(?:[Zz]|[+-](\d{2}):(\d{2}))$/;
const FILE_FIELDS = ["format", "version", "type", "id", "label", "created", "public", "kdf", "aead", "private"];
const KDF_FIELDS = ["alg", "m", "t", "p", "salt"];
const AEAD_FIELDS = ["alg", "nonce"];

/**
 * Error with a stable `code` for callers and tests. Codes: invalid-input, invalid-file,
 * invalid-id, unsupported-format, unsupported-version, unsupported-type, unsupported-kdf,
 * unsupported-aead, passphrase-required, identity-mismatch, authentication-failed.
 */
export class DeviceKeyError extends Error {
  constructor(code, message) {
    super(message);
    this.name = "DeviceKeyError";
    this.code = code;
  }
}

const invalidFile = message => new DeviceKeyError("invalid-file", message);
const invalidInput = message => new DeviceKeyError("invalid-input", message);
const subtle = () => globalThis.crypto.subtle;
const randomBytes = length => globalThis.crypto.getRandomValues(new Uint8Array(length));

/** Overwrites buffers we own. A detached or already-wiped view has length 0 and is skipped. */
function wipe(...buffers) {
  for (const buffer of buffers) if (buffer && buffer.byteLength) buffer.fill(0);
}

function bytesEqual(a, b) {
  if (a.length !== b.length) return false;
  let diff = 0;
  for (let i = 0; i < a.length; i++) diff |= a[i] ^ b[i];
  return diff === 0;
}

function isPlainObject(value) {
  if (value === null || typeof value !== "object" || Array.isArray(value)) return false;
  const proto = Object.getPrototypeOf(value);
  return proto === Object.prototype || proto === null;
}

/** Unpadded base64url, bit by bit, so no intermediate binary string holds the bytes. */
function b64urlEncode(bytes) {
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
}

/**
 * Strict decode of unpadded base64url. Refuses padding, the standard alphabet and non-canonical
 * trailing bits. The returned bytes belong to the caller, who must wipe them; on refusal the
 * partial output is wiped before the error is thrown.
 */
function decodeB64url(text, name) {
  if (typeof text !== "string" || !/^[A-Za-z0-9_-]*$/.test(text) || text.length % 4 === 1) {
    throw invalidFile(`${name} must be unpadded base64url`);
  }
  const bytes = new Uint8Array(Math.floor((text.length * 6) / 8));
  let value = 0;
  let bits = 0;
  let length = 0;
  for (let i = 0; i < text.length; i++) {
    value = (value << 6) | B64URL.indexOf(text[i]);
    bits += 6;
    if (bits >= 8) {
      bits -= 8;
      bytes[length++] = (value >>> bits) & 0xff;
    }
    value &= (1 << bits) - 1;
  }
  if (value !== 0) {
    wipe(bytes);
    throw invalidFile(`${name} is not canonical base64url`);
  }
  return bytes;
}

/** Decodes to exactly `length` bytes, or refuses. Wipes the bytes when refusing. */
function b64urlDecode(text, length, name = "value") {
  const bytes = decodeB64url(text, name);
  if (length !== undefined && bytes.length !== length) {
    const got = bytes.length;
    wipe(bytes);
    throw invalidFile(`${name} must be ${length} bytes, got ${got}`);
  }
  return bytes;
}

/** Validates a field's encoding and length without keeping the decoded bytes. */
function checkB64url(text, length, name) {
  wipe(b64urlDecode(text, length, name));
}

/** Rejects unpaired UTF-16 surrogates. JSON allows them, RFC 8785 does not. */
function assertWellFormed(text) {
  for (let i = 0; i < text.length; i++) {
    const unit = text.charCodeAt(i);
    if (unit >= 0xd800 && unit <= 0xdbff) {
      const next = text.charCodeAt(i + 1);
      if (!(next >= 0xdc00 && next <= 0xdfff)) throw invalidInput("text contains an unpaired surrogate (invalid Unicode)");
      i++;
    } else if (unit >= 0xdc00 && unit <= 0xdfff) {
      throw invalidInput("text contains an unpaired surrogate (invalid Unicode)");
    }
  }
}

/**
 * RFC 8785 (JCS) serialization. Object keys are sorted by UTF-16 code unit, which is the
 * default JavaScript sort. Numbers use ECMAScript's serialization, and non-finite numbers,
 * `undefined`, functions and non-plain objects are refused.
 */
function jcs(value) {
  if (value === null) return "null";
  switch (typeof value) {
    case "boolean":
      return String(value);
    case "number":
      if (!Number.isFinite(value)) throw invalidInput("JSON numbers must be finite");
      return JSON.stringify(value);
    case "string":
      assertWellFormed(value);
      return JSON.stringify(value);
    case "object":
      break;
    default:
      throw invalidInput(`value of type ${typeof value} cannot be serialized`);
  }
  if (Array.isArray(value)) return `[${value.map(item => jcs(item)).join(",")}]`;
  if (!isPlainObject(value)) throw invalidInput("only plain JSON objects can be serialized");
  const members = Object.keys(value).sort().map(key => {
    assertWellFormed(key);
    return `${JSON.stringify(key)}:${jcs(value[key])}`;
  });
  return `{${members.join(",")}}`;
}

/**
 * Associated data for the protected file's AES-GCM: canonical JSON (RFC 8785, UTF-8) of every
 * member except `private` and `aead.nonce`. Every other member is authenticated, including
 * unknown ones. The shape is not checked, so the fixture checks can call it on any object.
 * @returns {Uint8Array}
 */
export function keyFileAAD(file) {
  if (!isPlainObject(file)) throw invalidInput("key file must be a JSON object");
  const { private: omittedPrivate, ...header } = file;
  if (isPlainObject(header.aead)) {
    const { nonce: omittedNonce, ...algorithm } = header.aead;
    header.aead = algorithm;
  }
  return new TextEncoder().encode(jcs(header));
}

/** Base32 of the first 80 bits of SHA-256(public key), most significant bit first. */
function formatId(bytes) {
  let value = 0;
  let bits = 0;
  let encoded = "";
  for (const byte of bytes) {
    value = (value << 8) | byte;
    bits += 8;
    while (bits >= 5) {
      bits -= 5;
      encoded += CROCKFORD[(value >>> bits) & 31];
    }
    value &= (1 << bits) - 1;
  }
  return encoded.replace(/(.{4})(?!$)/g, "$1-");
}

/**
 * Device ID for a raw 32-byte Ed25519 public key: the first 80 bits of its SHA-256,
 * written as `XXXX-XXXX-XXXX-XXXX`.
 * @param {Uint8Array} publicKey
 * @returns {Promise<string>}
 */
export async function deriveId(publicKey) {
  if (!(publicKey instanceof Uint8Array) || publicKey.length !== PUBLIC_BYTES) {
    throw invalidInput("public key must be 32 raw Ed25519 bytes");
  }
  const digest = new Uint8Array(await subtle().digest("SHA-256", publicKey));
  return formatId(digest.subarray(0, 10));
}

/**
 * Normalizes a typed or pasted ID: ignores hyphens and case, reads O as 0 and I or L as 1,
 * and returns the canonical form. Throws invalid-id for anything else. It does not check
 * that the ID matches a key; use deriveId for that.
 * @param {string} input
 * @returns {string}
 */
export function canonicalId(input) {
  const invalid = () => new DeviceKeyError("invalid-id", "a device ID is 16 Crockford base32 characters, such as B7QK-2M9X-H4PD-R8TN");
  if (typeof input !== "string" || !/^[A-Za-z0-9-]*$/.test(input)) throw invalid();
  const normalized = input.replace(/-/g, "").toUpperCase().replace(/O/g, "0").replace(/[IL]/g, "1");
  if (!/^[0-9A-HJKMNP-TV-Z]{16}$/.test(normalized)) throw invalid();
  return normalized.replace(/(.{4})(?!$)/g, "$1-");
}

/**
 * A passphrase is omitted (undefined) for a plain file, or a string for a protected one. The
 * empty string is refused: hash-wasm 4.12.0, the Argon2id library in use, throws "Password must
 * be specified" for zero-length passwords, so "" cannot be a protected passphrase here. That is a
 * limit of the library, not of the format. Nonempty strings are encoded as exact UTF-8, with no
 * trimming and no Unicode normalization.
 */
function checkPassphrase(passphrase) {
  if (passphrase === undefined) return;
  if (typeof passphrase !== "string") throw invalidInput("passphrase must be a string; omit it for an unprotected key file");
  if (passphrase === "") throw invalidInput("passphrase must not be empty; omit it for an unprotected key file");
  try {
    assertWellFormed(passphrase);
  } catch {
    throw invalidInput("passphrase must be well-formed Unicode; a lone surrogate cannot be encoded exactly");
  }
}

/** RFC 3339 date-time with calendar and clock ranges checked. Fractions and offsets are allowed. */
function isTimestamp(text) {
  const match = RFC3339.exec(text);
  if (!match) return false;
  const [year, month, day, hour, minute, second] = match.slice(1, 7).map(Number);
  // A `Z` (or lowercase z) offset leaves the two offset groups undefined.
  const offsetHour = Number(match[7] ?? 0);
  const offsetMinute = Number(match[8] ?? 0);
  if (month < 1 || month > 12 || hour > 23 || minute > 59 || second > 60) return false;
  if (offsetHour > 23 || offsetMinute > 59) return false;
  const probe = new Date(0);
  probe.setUTCFullYear(year, month - 1, day);
  if (probe.getUTCFullYear() !== year || probe.getUTCMonth() !== month - 1 || probe.getUTCDate() !== day) return false;
  return !Number.isNaN(Date.parse(text.toUpperCase()));
}

function nowUtcSeconds() {
  return new Date().toISOString().replace(/\.\d{3}Z$/, "Z");
}

function checkExactFields(object, fields, name) {
  for (const key of Object.keys(object)) {
    if (!fields.includes(key)) throw invalidFile(`unknown field "${name}.${key}"`);
  }
  for (const key of fields) {
    if (!Object.hasOwn(object, key)) throw invalidFile(`missing field "${name}.${key}"`);
  }
}

/**
 * Structural check of a key file object. It does not decrypt or derive keys; identity is
 * checked later with the seed. Unknown fields are refused, so the authenticated data is
 * exactly the fields the format defines.
 */
function checkFile(file) {
  if (!isPlainObject(file)) throw invalidFile("key file must be a JSON object");
  if (file.format !== FORMAT) {
    throw new DeviceKeyError("unsupported-format", `not a BubblePlay device key file (format ${JSON.stringify(file.format)})`);
  }
  if (file.version !== VERSION) {
    throw new DeviceKeyError("unsupported-version", `key file version ${JSON.stringify(file.version)} is not supported; this app reads version 1`);
  }
  if (file.type !== TYPE) {
    throw new DeviceKeyError("unsupported-type", `key type ${JSON.stringify(file.type)} is not supported; expected "ed25519"`);
  }
  for (const key of Object.keys(file)) {
    if (!FILE_FIELDS.includes(key)) throw invalidFile(`unknown field "${key}"`);
  }
  for (const key of FILE_FIELDS) {
    if (!Object.hasOwn(file, key)) throw invalidFile(`missing field "${key}"`);
  }
  if (typeof file.id !== "string" || !CANONICAL_ID.test(file.id)) {
    throw new DeviceKeyError("invalid-id", 'id must be canonical "XXXX-XXXX-XXXX-XXXX" (uppercase Crockford)');
  }
  if (typeof file.label !== "string") throw invalidFile("label must be a string");
  if (typeof file.created !== "string" || !isTimestamp(file.created)) {
    throw invalidFile("created must be an RFC 3339 date-time, such as 2026-10-13T09:30:00Z");
  }
  checkB64url(file.public, PUBLIC_BYTES, "public");
  if (file.kdf === null) {
    if (file.aead !== null) throw invalidFile("aead must be null when kdf is null");
    checkB64url(file.private, SEED_BYTES, "private");
    return;
  }
  if (!isPlainObject(file.kdf) || !isPlainObject(file.aead)) {
    throw invalidFile("kdf and aead must both be objects, or both null");
  }
  checkExactFields(file.kdf, KDF_FIELDS, "kdf");
  if (file.kdf.alg !== KDF_ALG) {
    throw new DeviceKeyError("unsupported-kdf", `KDF ${JSON.stringify(file.kdf.alg)} is not supported; expected argon2id`);
  }
  if (file.kdf.m !== KDF_PARAMS.m || file.kdf.t !== KDF_PARAMS.t || file.kdf.p !== KDF_PARAMS.p) {
    throw new DeviceKeyError("unsupported-kdf", "unsupported Argon2id parameters; expected m=65536 t=3 p=1");
  }
  checkB64url(file.kdf.salt, SALT_BYTES, "kdf.salt");
  checkExactFields(file.aead, AEAD_FIELDS, "aead");
  if (file.aead.alg !== AEAD_ALG) {
    throw new DeviceKeyError("unsupported-aead", `AEAD ${JSON.stringify(file.aead.alg)} is not supported; expected aes-256-gcm`);
  }
  checkB64url(file.aead.nonce, NONCE_BYTES, "aead.nonce");
  checkB64url(file.private, SEED_BYTES + TAG_BYTES, "private");
}

/**
 * Shared entry point for parseKeyFile, unlockKeyFile and rewrapKeyFile: JSON text in,
 * structurally checked file object out. Does not decrypt.
 */
function parseStructure(text) {
  if (typeof text !== "string") throw invalidInput("key file must be JSON text");
  let file;
  try {
    file = JSON.parse(text);
  } catch {
    throw invalidFile("key file is not valid JSON");
  }
  // JSON.parse maps 1e400 to Infinity and accepts lone surrogate escapes; both are bad files.
  try {
    jcs(file);
  } catch {
    throw invalidFile("key file must use finite numbers and well-formed Unicode");
  }
  checkFile(file);
  return file;
}

/** A protected file needs its passphrase; a plain file must not get one. */
function checkUnlockPassphrase(file, passphrase) {
  checkPassphrase(passphrase);
  if (file.kdf === null && passphrase !== undefined) throw invalidInput("this key file has no passphrase");
  if (file.kdf !== null && passphrase === undefined) {
    throw new DeviceKeyError("passphrase-required", "this key file is protected; a passphrase is required");
  }
}

/** PKCS#8 for the seed, imported as Ed25519 for signing. The PKCS#8 copy is wiped on return. */
async function importSigningKey(seed, extractable) {
  const pkcs8 = new Uint8Array(PKCS8_ED25519_PREFIX.length + SEED_BYTES);
  try {
    pkcs8.set(PKCS8_ED25519_PREFIX);
    pkcs8.set(seed, PKCS8_ED25519_PREFIX.length);
    return await subtle().importKey("pkcs8", pkcs8, { name: "Ed25519" }, extractable, ["sign"]);
  } finally {
    wipe(pkcs8);
  }
}

/**
 * Raw public key for a seed. WebCrypto has no call that returns the public half of an
 * Ed25519 private key, so the seed is imported once as extractable and its public `x` is
 * read from the JWK. That extractable key is dropped here and never returned. The JWK also
 * carries `d` (the private seed) as an immutable string that cannot be wiped; it stays
 * local to this function. The long-lived signing key is a separate non-extractable import.
 */
async function publicKeyFromSeed(seed) {
  const extractable = await importSigningKey(seed, true);
  const jwk = await subtle().exportKey("jwk", extractable);
  const publicBytes = b64urlDecode(jwk.x, PUBLIC_BYTES, "public key");
  return publicBytes;
}

/** Derives the public key from the seed and refuses the file unless `public` and `id` match. */
async function verifyIdentity(file, seed) {
  const publicBytes = await publicKeyFromSeed(seed);
  try {
    const id = await deriveId(publicBytes);
    const filePublic = b64urlDecode(file.public, PUBLIC_BYTES, "public");
    try {
      if (!bytesEqual(publicBytes, filePublic) || id !== file.id) {
        throw new DeviceKeyError("identity-mismatch", "the key file's public key or ID does not match its private key");
      }
    } finally {
      wipe(filePublic);
    }
  } catch (error) {
    wipe(publicBytes);
    throw error;
  }
  return publicBytes;
}

/**
 * Argon2id through the injected KDF, then an AES-GCM key. The password copy and the KDF
 * output are wiped in `finally`; the AES key is imported as non-extractable.
 */
async function aesKeyFromPassphrase(kdf, passphrase, salt) {
  const password = new TextEncoder().encode(passphrase);
  let derived;
  try {
    derived = await kdf({ password, salt });
    if (!(derived instanceof Uint8Array) || derived.length !== KEY_BYTES) {
      throw invalidInput("the KDF must return 32 bytes");
    }
    return await subtle().importKey("raw", derived, "AES-GCM", false, ["encrypt", "decrypt"]);
  } finally {
    wipe(password, derived);
  }
}

/**
 * Returns the 32-byte seed. The caller must wipe it. A protected file needs its passphrase;
 * a wrong passphrase, or any change to the authenticated data, gives authentication-failed.
 */
async function recoverSeed(kdf, file, passphrase) {
  checkUnlockPassphrase(file, passphrase);
  if (file.kdf === null) return b64urlDecode(file.private, SEED_BYTES, "private");
  let salt;
  let nonce;
  let sealed;
  try {
    salt = b64urlDecode(file.kdf.salt, SALT_BYTES, "kdf.salt");
    nonce = b64urlDecode(file.aead.nonce, NONCE_BYTES, "aead.nonce");
    sealed = b64urlDecode(file.private, SEED_BYTES + TAG_BYTES, "private");
    const key = await aesKeyFromPassphrase(kdf, passphrase, salt);
    let seed;
    try {
      seed = new Uint8Array(await subtle().decrypt({ name: "AES-GCM", iv: nonce, additionalData: keyFileAAD(file), tagLength: 128 }, key, sealed));
      if (seed.length !== SEED_BYTES) throw invalidFile("decrypted seed must be 32 bytes");
      return seed;
    } catch (error) {
      wipe(seed);
      if (error.name === "OperationError") {
        throw new DeviceKeyError("authentication-failed", "wrong passphrase, or the key file was changed");
      }
      throw error;
    }
  } finally {
    wipe(salt, nonce, sealed);
  }
}

/** Builds the file object for a seed. Protected files get a fresh random salt and nonce. */
async function buildFile(kdf, { label, created, id, publicBytes, seed, passphrase }) {
  const header = { format: FORMAT, version: VERSION, type: TYPE, id, label, created, public: b64urlEncode(publicBytes) };
  if (passphrase === undefined) return { ...header, kdf: null, aead: null, private: b64urlEncode(seed) };
  const salt = randomBytes(SALT_BYTES);
  const nonce = randomBytes(NONCE_BYTES);
  const file = {
    ...header,
    kdf: { alg: KDF_ALG, ...KDF_PARAMS, salt: b64urlEncode(salt) },
    aead: { alg: AEAD_ALG, nonce: b64urlEncode(nonce) },
    private: "",
  };
  const key = await aesKeyFromPassphrase(kdf, passphrase, salt);
  const sealed = new Uint8Array(await subtle().encrypt({ name: "AES-GCM", iv: nonce, additionalData: keyFileAAD(file), tagLength: 128 }, key, seed));
  file.private = b64urlEncode(sealed);
  return file;
}

async function createKeyFileWith(kdf, { label, passphrase } = {}) {
  if (typeof label !== "string") throw invalidInput("label must be a string");
  assertWellFormed(label);
  checkPassphrase(passphrase);
  const seed = randomBytes(SEED_BYTES);
  let publicBytes;
  try {
    publicBytes = await publicKeyFromSeed(seed);
    const id = await deriveId(publicBytes);
    return await buildFile(kdf, { label, created: nowUtcSeconds(), id, publicBytes, seed, passphrase });
  } finally {
    wipe(seed, publicBytes);
  }
}

async function parseKeyFileWith(kdf, text, passphrase) {
  const file = parseStructure(text);
  checkUnlockPassphrase(file, passphrase);
  const seed = await recoverSeed(kdf, file, passphrase);
  try {
    wipe(await verifyIdentity(file, seed));
  } finally {
    wipe(seed);
  }
  return file;
}

async function unlockKeyFileWith(kdf, text, passphrase) {
  const file = parseStructure(text);
  const seed = await recoverSeed(kdf, file, passphrase);
  let publicKey;
  try {
    publicKey = await verifyIdentity(file, seed);
    const privateKey = await importSigningKey(seed, false);
    return { id: file.id, publicKey, privateKey };
  } catch (error) {
    wipe(publicKey);
    throw error;
  } finally {
    wipe(seed);
  }
}

async function rewrapKeyFileWith(kdf, text, passphrase, newPassphrase) {
  const file = parseStructure(text);
  checkPassphrase(newPassphrase);
  const seed = await recoverSeed(kdf, file, passphrase);
  let publicBytes;
  try {
    publicBytes = await verifyIdentity(file, seed);
    return await buildFile(kdf, { label: file.label, created: file.created, id: file.id, publicBytes, seed, passphrase: newPassphrase });
  } finally {
    wipe(seed, publicBytes);
  }
}

/**
 * Creates the API bound to a KDF. The KDF takes {password: Uint8Array, salt: Uint8Array} and
 * resolves to 32 bytes of Argon2id output. The default is the browser worker, which starts no
 * Worker until called, so importing this module is safe in Node.
 *
 * File arguments are JSON text; create and rewrap return objects for JSON.stringify.
 * - deriveId(publicKey: Uint8Array) -> Promise<string>
 * - canonicalId(input: string) -> string (accepts aliases; no key check)
 * - keyFileAAD(file: object) -> Uint8Array (RFC 8785 bytes that AES-GCM authenticates)
 * - createKeyFile({label: string, passphrase?: string}) -> Promise<file object>.
 *   Omitted passphrase = plain file; a nonempty string = protected. "" is refused.
 * - parseKeyFile(text, passphrase?) -> Promise<file object>. Verifies the seed-derived public
 *   key and ID. A protected file with no passphrase is refused with passphrase-required.
 * - unlockKeyFile(text, passphrase?) -> Promise<{id, publicKey: Uint8Array, privateKey: CryptoKey}>.
 *   The CryptoKey is non-extractable and signs Ed25519 only.
 * - rewrapKeyFile(text, passphrase?, newPassphrase?) -> Promise<file object>. Same identity, label
 *   and created time; fresh salt and nonce when protected.
 * Unlock and rewrap decrypt once, through the same structural parse as parseKeyFile.
 * @param {{kdf?: (input: {password: Uint8Array, salt: Uint8Array}) => Promise<Uint8Array>}} [options]
 */
export function createDeviceKeyApi({ kdf = deriveKeyInWorker } = {}) {
  if (typeof kdf !== "function") throw invalidInput("kdf must be a function");
  return {
    deriveId,
    canonicalId,
    keyFileAAD,
    parseKeyFile: (text, passphrase) => parseKeyFileWith(kdf, text, passphrase),
    createKeyFile: options => createKeyFileWith(kdf, options),
    unlockKeyFile: (text, passphrase) => unlockKeyFileWith(kdf, text, passphrase),
    rewrapKeyFile: (text, passphrase, newPassphrase) => rewrapKeyFileWith(kdf, text, passphrase, newPassphrase),
  };
}

// Named exports use the default API, whose KDF is the browser worker.
const defaultApi = createDeviceKeyApi();
export const parseKeyFile = defaultApi.parseKeyFile;
export const createKeyFile = defaultApi.createKeyFile;
export const unlockKeyFile = defaultApi.unlockKeyFile;
export const rewrapKeyFile = defaultApi.rewrapKeyFile;
