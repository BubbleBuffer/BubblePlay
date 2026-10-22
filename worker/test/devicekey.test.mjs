import { test } from "node:test";
import assert from "node:assert/strict";
import { createHash, webcrypto } from "node:crypto";
import { readFileSync } from "node:fs";
// Side-effect import defines globalThis.hashwasm. Node has no Web Worker, so the tests call
// the same Argon2id build the browser worker runs, with the same fixed parameters.
import "../public/vendor/hash-wasm/argon2.umd.min.js";
import {
  DeviceKeyError, canonicalId, createDeviceKeyApi, createKeyFile, deriveId, keyFileAAD,
  parseKeyFile, rewrapKeyFile, unlockKeyFile,
} from "../public/devicekey.js";

// TEST ONLY: every seed, passphrase, salt and nonce in the fixture is public and deterministic.
const fixture = JSON.parse(readFileSync(new URL("../../fixtures/device-keys-contract.json", import.meta.url), "utf8"));
const { subtle } = webcrypto;
const PASSPHRASE = fixture.keyFiles.passphrase.passphrase;
const hex = bytes => Buffer.from(bytes).toString("hex");
const fromB64 = text => Buffer.from(text, "base64url");
const utf8 = text => Buffer.from(text, "utf8");
const clone = value => JSON.parse(JSON.stringify(value));
const sha256 = bytes => createHash("sha256").update(bytes).digest();

// The same Argon2id call the browser worker makes: fixed parameters, 32-byte binary output.
const argon2Kdf = ({ password, salt }) => globalThis.hashwasm.argon2id({
  password, salt, iterations: 3, memorySize: 65536, parallelism: 1, hashLength: 32, outputType: "binary",
});

// Wraps the KDF so tests can see its inputs and count calls. `password` is a copy taken at call
// time; `passwordRef` is the live buffer, which the module wipes afterwards.
function spyKdf(kdf = argon2Kdf) {
  const spy = async input => {
    spy.calls.push({ password: input.password.slice(), salt: input.salt.slice(), passwordRef: input.password });
    return kdf(input);
  };
  spy.calls = [];
  return spy;
}

const kdf = spyKdf();
const api = createDeviceKeyApi({ kdf });

// Counts KDF calls made while `run` executes. The module calls the KDF synchronously before its
// first await, so the thunk must start after the count begins.
async function kdfCalls(spy, run) {
  const before = spy.calls.length;
  await run();
  return spy.calls.length - before;
}

async function signWith(privateKey, data) {
  return Buffer.from(await subtle.sign({ name: "Ed25519" }, privateKey, data));
}

async function verifyWith(publicKey, signature, data) {
  const key = await subtle.importKey("raw", publicKey, { name: "Ed25519" }, true, ["verify"]);
  return subtle.verify({ name: "Ed25519" }, key, signature, data);
}

async function expectError(promise, code, pattern) {
  await assert.rejects(promise, error => {
    assert.ok(error instanceof DeviceKeyError, `expected DeviceKeyError, got ${error?.name}: ${error?.message}`);
    assert.equal(error.code, code);
    if (pattern) assert.match(error.message, pattern);
    return true;
  });
}

// The fixture files as JSON text, which is what every unlock, parse and rewrap call takes.
const PLAIN = fixture.keyFiles.plain.file;
const PROTECTED = fixture.keyFiles.passphrase.file;
const PLAIN_TEXT = JSON.stringify(PLAIN);
const PROTECTED_TEXT = JSON.stringify(PROTECTED);
const PC_SIGNED = Buffer.from(fixture.handshake.pcSignedBytesHex, "hex");
const VIEWER_SIGNED = Buffer.from(fixture.handshake.viewerSignedBytesHex, "hex");

test("deriveId reproduces every fixture ID and the SHA-256 of each public key", async () => {
  for (const vector of fixture.ids) {
    const publicKey = fromB64(vector.public);
    assert.equal(hex(sha256(publicKey)), vector.sha256Hex, vector.name);
    assert.equal(await deriveId(new Uint8Array(publicKey)), vector.id, vector.name);
  }
});

test("deriveId rejects anything but 32 raw bytes", async () => {
  await expectError(deriveId(new Uint8Array(31)), "invalid-input");
  await expectError(deriveId(new Uint8Array(33)), "invalid-input");
  await expectError(deriveId("not bytes"), "invalid-input");
});

test("canonicalId accepts Crockford aliases, case and missing hyphens", () => {
  for (const vector of fixture.ids) {
    assert.equal(canonicalId(vector.id), vector.id);
    assert.equal(canonicalId(vector.id.toLowerCase()), vector.id);
    assert.equal(canonicalId(vector.id.replaceAll("-", "")), vector.id);
  }
  const alias = fixture.ids[2].parse;
  assert.equal(canonicalId(alias.input), alias.canonical);
  assert.equal(canonicalId("HN0B-W2W6-0QQ1-1M47"), "HN0B-W2W6-0QQ1-1M47");
  assert.equal(canonicalId("hnobw2w6oqqilm47"), "HN0B-W2W6-0QQ1-1M47");
});

test("canonicalId rejects wrong length, excluded letters, spaces and non-ASCII", () => {
  for (const input of ["", "HN0B-W2W6-0QQ1-1M4", "HN0B-W2W6-0QQ1-1M477", "HN0B-W2W6-0QU1-1M47", "HN0B W2W6 0QQ1 1M47", "HN0B-W2W6-0QQ1-1M4é", "ı0B-W2W6-0QQ1-1M47", 1234]) {
    assert.throws(() => canonicalId(input), error => error instanceof DeviceKeyError && error.code === "invalid-id", JSON.stringify(input));
  }
});

test("keyFileAAD matches the fixture bytes, ignores member order and omits private and nonce", () => {
  const vector = fixture.keyFiles.passphrase;
  assert.equal(Buffer.from(keyFileAAD(PROTECTED)).toString("utf8"), vector.aadUtf8);
  assert.equal(hex(keyFileAAD(PROTECTED)), vector.aadHex);
  assert.deepEqual(keyFileAAD(Object.fromEntries(Object.entries(PROTECTED).reverse())), keyFileAAD(PROTECTED));
  const changedSecret = { ...PROTECTED, private: "AAAA", aead: { ...PROTECTED.aead, nonce: "AAAAAAAAAAAAAAAA" } };
  assert.deepEqual(keyFileAAD(changedSecret), keyFileAAD(PROTECTED));
  assert.notDeepEqual(keyFileAAD({ ...PROTECTED, label: "other" }), keyFileAAD(PROTECTED));
});

test("keyFileAAD authenticates unknown members and uses RFC 8785 string escaping", () => {
  const withExtra = { ...PROTECTED, extra: { z: 1, a: [true, null] } };
  assert.match(Buffer.from(keyFileAAD(withExtra)).toString("utf8"), /"extra":\{"a":\[true,null\],"z":1\}/);
  assert.notDeepEqual(keyFileAAD(withExtra), keyFileAAD(PROTECTED));
  assert.equal(Buffer.from(keyFileAAD({ s: "\u0001\u001f\"\\é€" })).toString("utf8"), '{"s":"\\u0001\\u001f\\"\\\\é€"}');
});

test("keyFileAAD refuses invalid Unicode, non-finite numbers and non-JSON values", () => {
  assert.throws(() => keyFileAAD({ label: "\ud800" }), /surrogate/);
  assert.throws(() => keyFileAAD({ label: "\udc00x" }), /surrogate/);
  assert.throws(() => keyFileAAD({ ["bad\ud834"]: 1 }), /surrogate/);
  assert.throws(() => keyFileAAD({ n: Number.NaN }), /finite/);
  assert.throws(() => keyFileAAD({ n: Number.POSITIVE_INFINITY }), /finite/);
  assert.throws(() => keyFileAAD({ u: undefined }), DeviceKeyError);
  assert.throws(() => keyFileAAD({ f: () => 1 }), DeviceKeyError);
  assert.throws(() => keyFileAAD(new Map()), DeviceKeyError);
});

test("plain fixture parses, unlocks and signs exactly as the fixture signature", async () => {
  assert.deepEqual(await api.parseKeyFile(PLAIN_TEXT), PLAIN);
  const unlocked = await api.unlockKeyFile(PLAIN_TEXT);
  assert.equal(unlocked.id, fixture.keyFiles.plain.file.id);
  assert.equal(hex(unlocked.publicKey), hex(fromB64(PLAIN.public)));
  assert.equal(unlocked.privateKey.type, "private");
  assert.equal(unlocked.privateKey.algorithm.name, "Ed25519");
  const signature = await signWith(unlocked.privateKey, PC_SIGNED);
  assert.equal(hex(signature), hex(fromB64(fixture.handshake.pcSignature)));
  assert.equal(await verifyWith(unlocked.publicKey, signature, PC_SIGNED), true);
});

test("protected fixture: parse without a passphrase is refused with passphrase-required and calls no KDF", async () => {
  const spy = spyKdf();
  const local = createDeviceKeyApi({ kdf: spy });
  await expectError(local.parseKeyFile(PROTECTED_TEXT), "passphrase-required", /passphrase is required/);
  await expectError(local.parseKeyFile(PROTECTED_TEXT, undefined), "passphrase-required");
  assert.equal(spy.calls.length, 0);
});

test("protected fixture: parse with the passphrase verifies identity; unlock signs as the fixture", async () => {
  assert.deepEqual(await api.parseKeyFile(PROTECTED_TEXT, PASSPHRASE), PROTECTED);
  const unlocked = await api.unlockKeyFile(PROTECTED_TEXT, PASSPHRASE);
  assert.equal(unlocked.id, PROTECTED.id);
  assert.equal(hex(unlocked.publicKey), hex(fromB64(PROTECTED.public)));
  const signature = await signWith(unlocked.privateKey, VIEWER_SIGNED);
  assert.equal(hex(signature), hex(fromB64(fixture.handshake.viewerSignature)));
});

test("Argon2id through the injected KDF reproduces the fixture key, from the 16-byte salt", async () => {
  const { passphrase, kdfKeyHex } = fixture.keyFiles.passphrase;
  const derived = await argon2Kdf({ password: utf8(passphrase), salt: fromB64(PROTECTED.kdf.salt) });
  assert.equal(hex(derived), kdfKeyHex);
});

test("protected fixture: wrong passphrase and tampered AAD fail with authentication-failed", async () => {
  const [wrong, tampered] = fixture.keyFiles.failures;
  await expectError(api.parseKeyFile(JSON.stringify(wrong.file), wrong.passphrase), "authentication-failed", /wrong passphrase/);
  await expectError(api.unlockKeyFile(JSON.stringify(wrong.file), wrong.passphrase), "authentication-failed");
  await expectError(api.parseKeyFile(JSON.stringify(tampered.file), tampered.passphrase), "authentication-failed");
  await expectError(api.unlockKeyFile(JSON.stringify(tampered.file), tampered.passphrase), "authentication-failed");
});

test("a tampered ID, label, created time or KDF salt fails on decryption, before identity is checked", async () => {
  await expectError(api.unlockKeyFile(JSON.stringify({ ...PROTECTED, label: "Renamed" }), PASSPHRASE), "authentication-failed");
  await expectError(api.unlockKeyFile(JSON.stringify({ ...PROTECTED, id: fixture.ids[1].id }), PASSPHRASE), "authentication-failed");
  await expectError(api.unlockKeyFile(JSON.stringify({ ...PROTECTED, created: "2026-10-13T09:30:00.000Z" }), PASSPHRASE), "authentication-failed");
  const salt = clone(PROTECTED);
  salt.kdf.salt = Buffer.alloc(16, 7).toString("base64url");
  await expectError(api.unlockKeyFile(JSON.stringify(salt), PASSPHRASE), "authentication-failed");
});

test("the KDF receives the exact UTF-8 password and a 16-byte salt, and the password is wiped afterwards", async () => {
  const spy = spyKdf();
  const local = createDeviceKeyApi({ kdf: spy });
  const passphrase = "pässwörd €";
  const file = JSON.stringify(await local.createKeyFile({ label: "Wipe check", passphrase }));
  const [call] = spy.calls;
  assert.equal(call.salt.length, 16);
  assert.equal(hex(call.password), hex(utf8(passphrase)));
  assert.ok(call.passwordRef.every(byte => byte === 0), "password buffer is wiped after use");
  await local.unlockKeyFile(file, passphrase);
  assert.equal(spy.calls.length, 2);
  assert.ok(spy.calls[1].passwordRef.every(byte => byte === 0), "unlock wipes the password too");
});

test("passphrases are exact UTF-8: NFC and NFD differ, and leading or trailing spaces are kept", async () => {
  const spy = spyKdf();
  const local = createDeviceKeyApi({ kdf: spy });
  const nfc = "café";
  const nfd = "café";
  const file = JSON.stringify(await local.createKeyFile({ label: "Unicode", passphrase: nfc }));
  assert.equal(hex(spy.calls.at(-1).password), hex(utf8(nfc)));
  await local.unlockKeyFile(file, nfc);
  await expectError(local.unlockKeyFile(file, nfd), "authentication-failed");

  const spaced = JSON.stringify(await local.createKeyFile({ label: "Spaces", passphrase: " pass " }));
  assert.equal(hex(spy.calls.at(-1).password), hex(utf8(" pass ")));
  await local.unlockKeyFile(spaced, " pass ");
  await expectError(local.unlockKeyFile(spaced, "pass"), "authentication-failed");
  await expectError(local.unlockKeyFile(spaced, " pass"), "authentication-failed");
});

test("a passphrase that is not well-formed Unicode is refused before any KDF call", async () => {
  const spy = spyKdf();
  const local = createDeviceKeyApi({ kdf: spy });
  await expectError(local.createKeyFile({ label: "x", passphrase: "\ud800" }), "invalid-input", /well-formed Unicode/);
  await expectError(local.parseKeyFile(PROTECTED_TEXT, "bad\udc00"), "invalid-input", /well-formed Unicode/);
  assert.equal(spy.calls.length, 0);
});

// hash-wasm 4.12.0 throws "Password must be specified" for a zero-length password. The empty
// string is refused on every path before the KDF runs, so no code reaches that error.
test("the real Argon2id library rejects a zero-length password, which is why \"\" is refused", async () => {
  await assert.rejects(async () => globalThis.hashwasm.argon2id({
    password: new Uint8Array(0), salt: new Uint8Array(16).fill(1), iterations: 3, memorySize: 65536,
    parallelism: 1, hashLength: 32, outputType: "binary",
  }), /Password must be specified/);
});

test("an empty passphrase is refused with invalid-input on every path, and the real KDF is never called", async () => {
  const spy = spyKdf();
  const local = createDeviceKeyApi({ kdf: spy });
  const rule = /must not be empty; omit it for an unprotected key file/;
  await expectError(local.createKeyFile({ label: "Empty", passphrase: "" }), "invalid-input", rule);
  await expectError(local.parseKeyFile(PROTECTED_TEXT, ""), "invalid-input", rule);
  await expectError(local.unlockKeyFile(PROTECTED_TEXT, ""), "invalid-input", rule);
  await expectError(local.rewrapKeyFile(PROTECTED_TEXT, "", "next"), "invalid-input", rule);
  await expectError(local.rewrapKeyFile(PROTECTED_TEXT, PASSPHRASE, ""), "invalid-input", rule);
  assert.equal(spy.calls.length, 0);
});

test("a KDF that returns the wrong length is refused, and the password is still wiped", async () => {
  let seen;
  const bad = createDeviceKeyApi({ kdf: async input => { seen = input.password; return new Uint8Array(31); } });
  await expectError(bad.createKeyFile({ label: "short", passphrase: "x" }), "invalid-input", /32 bytes/);
  assert.ok(seen.every(byte => byte === 0));
});

test("a KDF that rejects propagates its error and wipes the password", async () => {
  let seen;
  const failing = createDeviceKeyApi({ kdf: async input => { seen = input.password; throw new Error("worker gone"); } });
  await assert.rejects(failing.parseKeyFile(PROTECTED_TEXT, PASSPHRASE), /worker gone/);
  assert.ok(seen.every(byte => byte === 0));
});

test("parse, unlock and rewrap take JSON text: objects, buffers and other values are invalid-input", async () => {
  const spy = spyKdf();
  const local = createDeviceKeyApi({ kdf: spy });
  await expectError(local.parseKeyFile(PLAIN), "invalid-input", /JSON text/);
  await expectError(local.parseKeyFile(undefined), "invalid-input", /JSON text/);
  await expectError(local.unlockKeyFile(PLAIN), "invalid-input", /JSON text/);
  await expectError(local.unlockKeyFile(Buffer.from(PLAIN_TEXT)), "invalid-input", /JSON text/);
  await expectError(local.unlockKeyFile(PROTECTED, PASSPHRASE), "invalid-input", /JSON text/);
  await expectError(local.rewrapKeyFile(PROTECTED, PASSPHRASE, "next"), "invalid-input", /JSON text/);
  assert.equal(spy.calls.length, 0);
});

test("unlock and parse decrypt once; rewrap runs the KDF once per passphrase it uses", async () => {
  const spy = spyKdf();
  const local = createDeviceKeyApi({ kdf: spy });
  assert.equal(await kdfCalls(spy, () => local.parseKeyFile(PROTECTED_TEXT, PASSPHRASE)), 1);
  assert.equal(await kdfCalls(spy, () => local.unlockKeyFile(PROTECTED_TEXT, PASSPHRASE)), 1);
  assert.equal(await kdfCalls(spy, () => local.unlockKeyFile(PLAIN_TEXT)), 0);

  // Protected -> protected: one decrypt with the old passphrase, one encrypt with the new.
  const before = spy.calls.length;
  await local.rewrapKeyFile(PROTECTED_TEXT, PASSPHRASE, "next pass");
  assert.equal(spy.calls.length - before, 2);
  assert.equal(hex(spy.calls.at(-2).password), hex(utf8(PASSPHRASE)));
  assert.equal(hex(spy.calls.at(-1).password), hex(utf8("next pass")));

  assert.equal(await kdfCalls(spy, () => local.rewrapKeyFile(PROTECTED_TEXT, PASSPHRASE, undefined)), 1);
  assert.equal(await kdfCalls(spy, () => local.rewrapKeyFile(PLAIN_TEXT, undefined, PASSPHRASE)), 1);
});

test("unknown format, version and type are refused with clear messages", async () => {
  const text = file => JSON.stringify(file);
  await expectError(api.parseKeyFile(text({ ...PLAIN, format: "other-format" })), "unsupported-format", /not a BubblePlay device key file/);
  await expectError(api.parseKeyFile(text({ ...PLAIN, version: 2 })), "unsupported-version", /version 2 .* version 1/);
  await expectError(api.parseKeyFile(text({ ...PLAIN, type: "p256" })), "unsupported-type", /"p256" is not supported/);
  await expectError(api.unlockKeyFile(text({ ...PLAIN, version: "1" })), "unsupported-version");
});

// Each case changes one thing; the expected code and message name that thing, so a case
// cannot pass for the wrong reason.
const PLAIN_CASES = [
  ["not JSON", () => "{", "invalid-file", /not valid JSON/],
  ["JSON null", () => "null", "invalid-file", /JSON object/],
  ["array", () => "[]", "invalid-file", /JSON object/],
  ["extra member", () => JSON.stringify({ ...PLAIN, extra: 1 }), "invalid-file", /unknown field "extra"/],
  ["missing label", () => JSON.stringify(Object.fromEntries(Object.entries(PLAIN).filter(([k]) => k !== "label"))), "invalid-file", /missing field "label"/],
  ["lowercase id", () => JSON.stringify({ ...PLAIN, id: PLAIN.id.toLowerCase() }), "invalid-id", /canonical/],
  ["alias letter in id", () => JSON.stringify({ ...PLAIN, id: "77VH-7M56-8GJK-Y12O" }), "invalid-id", /canonical/],
  ["unhyphenated id", () => JSON.stringify({ ...PLAIN, id: PLAIN.id.replaceAll("-", "") }), "invalid-id", /canonical/],
  ["id not matching the key", () => JSON.stringify({ ...PLAIN, id: "77VH-7M56-8GJK-Y12K" }), "identity-mismatch", /does not match/],
  ["label not a string", () => JSON.stringify({ ...PLAIN, label: 7 }), "invalid-file", /label must be a string/],
  ["label with lone surrogate", () => '{"format":"bubbleplay-device-key","version":1,"type":"ed25519","id":"77VH-7M56-8GJK-Y12J","label":"\\ud800","created":"2026-10-13T09:30:00Z","public":"PUAXw-hDiVqStwqnTRt-vJyYLM8uxJaMwM1V8Sr0Zgw","kdf":null,"aead":null,"private":"TM0Imyj_ltqdtsNG7BFOD1uKMZ81q6Yk2oz27U-4pvs"}', "invalid-file", /well-formed Unicode/],
  ["public 30 bytes", () => JSON.stringify({ ...PLAIN, public: PLAIN.public.slice(0, -3) }), "invalid-file", /public must be 32 bytes, got 30/],
  ["public padded", () => JSON.stringify({ ...PLAIN, public: `${PLAIN.public}=` }), "invalid-file", /unpadded base64url/],
  ["public standard alphabet", () => JSON.stringify({ ...PLAIN, public: PLAIN.public.replaceAll("-", "+") }), "invalid-file", /unpadded base64url/],
  ["public non-canonical trailing bits", () => JSON.stringify({ ...PLAIN, public: `${PLAIN.public.slice(0, -1)}p` }), "invalid-file", /not canonical base64url/],
  ["created not a real date", () => JSON.stringify({ ...PLAIN, created: "2026-02-31T00:00:00Z" }), "invalid-file", /created must be/],
  ["plain private 30 bytes", () => JSON.stringify({ ...PLAIN, private: PLAIN.private.slice(0, -3) }), "invalid-file", /private must be 32 bytes, got 30/],
  ["aead object with null kdf", () => JSON.stringify({ ...PLAIN, aead: { alg: "aes-256-gcm", nonce: "EBESExQVFhcYGRob" } }), "invalid-file", /aead must be null/],
  ["kdf object with null aead", () => JSON.stringify({ ...PLAIN, kdf: PROTECTED.kdf }), "invalid-file", /both be objects, or both null/],
];

for (const [name, text, code, pattern] of PLAIN_CASES) {
  test(`malformed plain file: ${name}`, async () => {
    await expectError(api.parseKeyFile(text()), code, pattern);
  });
}

const PROTECTED_CASES = [
  ["Argon2i", f => { f.kdf.alg = "argon2i"; }, "unsupported-kdf", /argon2id/],
  ["memory 32 MiB", f => { f.kdf.m = 32768; }, "unsupported-kdf", /m=65536/],
  ["passes 4", f => { f.kdf.t = 4; }, "unsupported-kdf", /t=3/],
  ["lanes 2", f => { f.kdf.p = 2; }, "unsupported-kdf", /p=1/],
  ["salt 15 bytes", f => { f.kdf.salt = f.kdf.salt.slice(0, -2); }, "invalid-file", /kdf\.salt must be 16 bytes, got 15/],
  ["kdf extra member", f => { f.kdf.version = 19; }, "invalid-file", /unknown field "kdf.version"/],
  ["AES-128-GCM", f => { f.aead.alg = "aes-128-gcm"; }, "unsupported-aead", /aes-256-gcm/],
  ["nonce 9 bytes", f => { f.aead.nonce = f.aead.nonce.slice(0, -4); }, "invalid-file", /aead\.nonce must be 12 bytes, got 9/],
  ["private 45 bytes", f => { f.private = f.private.slice(0, -4); }, "invalid-file", /private must be 48 bytes, got 45/],
  ["private 32 bytes (plain length)", f => { f.private = PLAIN.private; }, "invalid-file", /private must be 48 bytes, got 32/],
  ["aead null with kdf object", f => { f.aead = null; }, "invalid-file", /both be objects, or both null/],
];

for (const [name, change, code, pattern] of PROTECTED_CASES) {
  test(`malformed protected file: ${name}`, async () => {
    const file = clone(PROTECTED);
    change(file);
    // Structural refusals happen before the KDF runs, so the passphrase is never used here.
    const spy = spyKdf();
    await expectError(createDeviceKeyApi({ kdf: spy }).parseKeyFile(JSON.stringify(file), PASSPHRASE), code, pattern);
    assert.equal(spy.calls.length, 0);
  });
}

test("a file whose ID or public key does not match the seed is refused", async () => {
  const wrongId = { ...PLAIN, id: fixture.ids[0].id };
  await expectError(api.parseKeyFile(JSON.stringify(wrongId)), "identity-mismatch", /does not match/);
  await expectError(api.unlockKeyFile(JSON.stringify(wrongId)), "identity-mismatch");
  const wrongPublic = { ...PLAIN, public: fixture.ids[0].public };
  await expectError(api.parseKeyFile(JSON.stringify(wrongPublic)), "identity-mismatch");
  await expectError(api.unlockKeyFile(JSON.stringify(wrongPublic)), "identity-mismatch");
});

test("created accepts RFC 3339 with fractions and offsets, and refuses what is not RFC 3339", async () => {
  for (const created of ["2026-10-13T09:30:00.123Z", "2026-10-13T11:30:00+02:00", "2026-10-13t09:30:00.5-05:30", "2026-10-13T09:30:00z", "2026-10-13T09:30:00.000Z"]) {
    const parsed = await api.parseKeyFile(JSON.stringify({ ...PLAIN, created }));
    assert.equal(parsed.created, created);
  }
  for (const created of ["2026-10-13T09:30:00", "2026-10-13 09:30:00Z", "2026-10-13T09:30Z", "2026-10-13T09:30:00+0200", "2026-10-13T24:00:00Z", "2026-13-01T00:00:00Z", "2026-02-29T00:00:00Z"]) {
    await expectError(api.parseKeyFile(JSON.stringify({ ...PLAIN, created })), "invalid-file", /created must be/);
  }
});

test("labels are any string: empty and longer than 64 characters are accepted", async () => {
  assert.equal((await api.parseKeyFile(JSON.stringify({ ...PLAIN, label: "" }))).label, "");
  const long = "x".repeat(200);
  assert.equal((await api.parseKeyFile(JSON.stringify({ ...PLAIN, label: long }))).label, long);

  const empty = await api.createKeyFile({ label: "" });
  assert.equal(empty.label, "");
  const longFile = await api.createKeyFile({ label: long, passphrase: "p" });
  assert.equal(longFile.label, long);
  assert.equal((await api.unlockKeyFile(JSON.stringify(longFile), "p")).id, longFile.id);

  await expectError(api.createKeyFile({}), "invalid-input", /label must be a string/);
  await expectError(api.createKeyFile({ label: 5 }), "invalid-input", /label must be a string/);
  await expectError(api.createKeyFile({ label: "\ud800" }), "invalid-input", /surrogate/);
});

test("passphrase rules: plain files reject one, protected files need one to unlock, types are checked", async () => {
  await expectError(api.parseKeyFile(PLAIN_TEXT, "anything"), "invalid-input", /no passphrase/);
  await expectError(api.unlockKeyFile(PLAIN_TEXT, "anything"), "invalid-input", /no passphrase/);
  await expectError(api.unlockKeyFile(PROTECTED_TEXT), "passphrase-required", /passphrase is required/);
  await expectError(api.parseKeyFile(PROTECTED_TEXT, 123), "invalid-input", /string/);
  await expectError(api.createKeyFile({ label: "x", passphrase: null }), "invalid-input", /string/);
});

test("createKeyFile: plain file has the spec fields and round-trips through text", async () => {
  const file = await api.createKeyFile({ label: "Test phone" });
  assert.deepEqual(Object.keys(file), ["format", "version", "type", "id", "label", "created", "public", "kdf", "aead", "private"]);
  assert.equal(file.kdf, null);
  assert.equal(file.aead, null);
  assert.match(file.created, /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$/);
  assert.equal(fromB64(file.private).length, 32);
  const text = JSON.stringify(file);
  assert.deepEqual(await api.parseKeyFile(text), file);
  const unlocked = await api.unlockKeyFile(text);
  assert.equal(unlocked.id, file.id);
  assert.equal(await api.deriveId(unlocked.publicKey), file.id);
  assert.equal(hex(unlocked.publicKey), hex(fromB64(file.public)));
  const message = utf8("hello");
  assert.equal(await verifyWith(unlocked.publicKey, await signWith(unlocked.privateKey, message), message), true);
});

test("createKeyFile: protected file uses argon2id, a 16-byte salt and a 12-byte nonce, and unlocks only with the exact passphrase", async () => {
  const file = await api.createKeyFile({ label: "Laptop", passphrase: "correct horse" });
  assert.deepEqual(file.kdf, { alg: "argon2id", m: 65536, t: 3, p: 1, salt: file.kdf.salt });
  assert.equal(fromB64(file.kdf.salt).length, 16);
  assert.equal(fromB64(file.aead.nonce).length, 12);
  assert.equal(fromB64(file.private).length, 48);
  const text = JSON.stringify(file);
  assert.deepEqual(await api.parseKeyFile(text, "correct horse"), file);
  const unlocked = await api.unlockKeyFile(text, "correct horse");
  assert.equal(unlocked.id, file.id);
  await expectError(api.unlockKeyFile(text, "Correct horse"), "authentication-failed");
});

test("each creation draws a new seed, salt and nonce", async () => {
  const first = await api.createKeyFile({ label: "a", passphrase: "p" });
  const second = await api.createKeyFile({ label: "a", passphrase: "p" });
  assert.notEqual(first.id, second.id);
  assert.notEqual(first.kdf.salt, second.kdf.salt);
  assert.notEqual(first.aead.nonce, second.aead.nonce);
});

test("rewrap protected -> protected: fresh salt and nonce, same identity, old passphrase stops working", async () => {
  const original = await api.createKeyFile({ label: "Keeper", passphrase: "old pass" });
  const originalText = JSON.stringify(original);
  const once = await api.rewrapKeyFile(originalText, "old pass", "new pass");
  const twice = await api.rewrapKeyFile(originalText, "old pass", "new pass");
  for (const next of [once, twice]) {
    assert.notEqual(next.kdf.salt, original.kdf.salt);
    assert.notEqual(next.aead.nonce, original.aead.nonce);
    assert.equal(next.id, original.id);
    assert.equal(next.public, original.public);
    assert.equal(next.label, original.label);
    assert.equal(next.created, original.created);
  }
  assert.notEqual(once.kdf.salt, twice.kdf.salt);
  assert.notEqual(once.aead.nonce, twice.aead.nonce);
  await expectError(api.unlockKeyFile(JSON.stringify(once), "old pass"), "authentication-failed");
  const before = await api.unlockKeyFile(originalText, "old pass");
  const after = await api.unlockKeyFile(JSON.stringify(once), "new pass");
  assert.equal(after.id, before.id);
  const message = utf8("same key");
  assert.equal(hex(await signWith(after.privateKey, message)), hex(await signWith(before.privateKey, message)));
});

test("rewrap keeps a fractional created time, and refuses unknown members the same way parse does", async () => {
  const fractional = { ...PLAIN, created: "2026-10-13T09:30:00.250+02:00" };
  const rewrapped = await api.rewrapKeyFile(JSON.stringify(fractional), undefined, PASSPHRASE);
  assert.equal(rewrapped.created, fractional.created);
  await expectError(api.parseKeyFile(JSON.stringify({ ...PLAIN, extra: { z: 1 } })), "invalid-file", /unknown field "extra"/);
  await expectError(api.rewrapKeyFile(JSON.stringify({ ...PLAIN, extra: { z: 1 } }), undefined, PASSPHRASE), "invalid-file", /unknown field "extra"/);
});

test("rewrap plain -> protected and protected -> plain keep the identity", async () => {
  const protectedFile = await api.rewrapKeyFile(PLAIN_TEXT, undefined, PASSPHRASE);
  assert.notEqual(protectedFile.kdf, null);
  assert.equal(fromB64(protectedFile.kdf.salt).length, 16);
  assert.equal(protectedFile.id, PLAIN.id);
  assert.equal(protectedFile.public, PLAIN.public);
  assert.equal(protectedFile.created, PLAIN.created);
  assert.equal(protectedFile.label, PLAIN.label);
  const fromProtected = await api.unlockKeyFile(JSON.stringify(protectedFile), PASSPHRASE);
  assert.equal(hex(await signWith(fromProtected.privateKey, PC_SIGNED)), hex(fromB64(fixture.handshake.pcSignature)));

  const back = await api.rewrapKeyFile(JSON.stringify(protectedFile), PASSPHRASE, undefined);
  assert.equal(back.kdf, null);
  assert.equal(back.aead, null);
  assert.equal(hex(fromB64(back.private)), fixture.keyFiles.plain.seedHex);
  assert.equal(back.id, PLAIN.id);
});

test("rewrap refuses a wrong current passphrase, a passphrase for a plain file, and invalid types", async () => {
  await expectError(api.rewrapKeyFile(PROTECTED_TEXT, "nope", "new"), "authentication-failed");
  await expectError(api.rewrapKeyFile(PLAIN_TEXT, "unexpected", "new"), "invalid-input", /no passphrase/);
  await expectError(api.rewrapKeyFile(PROTECTED_TEXT, PASSPHRASE, 5), "invalid-input", /string/);
  await expectError(api.rewrapKeyFile(PROTECTED_TEXT, undefined, "new"), "passphrase-required");
});

test("non-extractable: unlocked CryptoKey cannot be exported by any format", async () => {
  const unlocked = await api.unlockKeyFile(PROTECTED_TEXT, PASSPHRASE);
  assert.equal(unlocked.privateKey.extractable, false);
  assert.deepEqual(unlocked.privateKey.usages, ["sign"]);
  await assert.rejects(subtle.exportKey("jwk", unlocked.privateKey));
  await assert.rejects(subtle.exportKey("pkcs8", unlocked.privateKey));
  const plain = await api.unlockKeyFile(PLAIN_TEXT);
  assert.equal(plain.privateKey.extractable, false);
  await assert.rejects(subtle.exportKey("pkcs8", plain.privateKey));
});

test("a rewrap or create exports only the short-lived extractable public-key import, never the long-lived CryptoKey", async () => {
  const exports = [];
  const original = subtle.exportKey;
  subtle.exportKey = function (format, key) {
    exports.push({ format, extractable: key.extractable });
    return original.call(this, format, key);
  };
  try {
    await api.rewrapKeyFile(PROTECTED_TEXT, PASSPHRASE, "fresh");
    await api.createKeyFile({ label: "export check" });
  } finally {
    subtle.exportKey = original;
  }
  assert.ok(exports.length >= 2);
  assert.ok(exports.every(entry => entry.format === "jwk" && entry.extractable === true), JSON.stringify(exports));
});

test("no key material touches localStorage or sessionStorage", async () => {
  const touched = [];
  const trap = name => ({ setItem: (...args) => touched.push([name, "set", args[0]]), getItem: () => null, removeItem: () => {}, clear: () => {}, length: 0, key: () => null });
  const descriptors = ["localStorage", "sessionStorage"].map(name => [name, Object.getOwnPropertyDescriptor(globalThis, name)]);
  for (const name of ["localStorage", "sessionStorage"]) {
    Object.defineProperty(globalThis, name, { value: trap(name), configurable: true, writable: true });
  }
  try {
    const created = JSON.stringify(await api.createKeyFile({ label: "Store check", passphrase: "p" }));
    await api.parseKeyFile(created, "p");
    await api.unlockKeyFile(created, "p");
    await api.rewrapKeyFile(created, "p", undefined);
  } finally {
    for (const [name, descriptor] of descriptors) {
      if (descriptor) Object.defineProperty(globalThis, name, descriptor);
      else delete globalThis[name];
    }
  }
  assert.deepEqual(touched, []);
});

// Node has no Web Worker. This shim runs the same Argon2id call as devicekey-kdf-worker.js, with
// the same fixed options, and delivers the key through onmessage like the real worker does.
function installWorkerShim() {
  const previous = Object.getOwnPropertyDescriptor(globalThis, "Worker");
  const shim = { constructed: [] };
  class ShimWorker {
    constructor(url, options) {
      shim.constructed.push({ url: String(url), options });
      this.onmessage = null;
      this.onerror = null;
      this.onmessageerror = null;
    }
    postMessage({ password, salt }) {
      // The main thread wipes its copies once the promise settles, so copy the inputs now.
      const input = { password: new Uint8Array(password), salt: new Uint8Array(salt) };
      Promise.resolve()
        .then(() => argon2Kdf(input))
        .then(
          key => this.onmessage({ data: { key } }),
          error => this.onerror({ preventDefault() {}, error }),
        );
    }
    terminate() {}
  }
  Object.defineProperty(globalThis, "Worker", { value: ShimWorker, configurable: true, writable: true });
  return {
    shim,
    restore() {
      if (previous) Object.defineProperty(globalThis, "Worker", previous);
      else delete globalThis.Worker;
    },
  };
}

test("named exports need a Worker for protected files and say so when none exists", async () => {
  if (typeof globalThis.Worker === "function") return;
  await assert.rejects(parseKeyFile(PROTECTED_TEXT, PASSPHRASE), error => {
    assert.ok(error instanceof Error && !(error instanceof DeviceKeyError));
    assert.match(error.message, /requires Web Worker support/);
    return true;
  });
});

test("named exports run the browser-worker KDF path, with the Worker shim, on plain and protected text", async () => {
  const workerBefore = typeof globalThis.Worker;
  const { shim, restore } = installWorkerShim();
  try {
    assert.deepEqual(await parseKeyFile(PROTECTED_TEXT, PASSPHRASE), PROTECTED);
    await expectError(parseKeyFile(PROTECTED_TEXT), "passphrase-required");
    const unlockedProtected = await unlockKeyFile(PROTECTED_TEXT, PASSPHRASE);
    assert.equal(unlockedProtected.id, PROTECTED.id);
    assert.equal(hex(await signWith(unlockedProtected.privateKey, VIEWER_SIGNED)), hex(fromB64(fixture.handshake.viewerSignature)));
    const unlockedPlain = await unlockKeyFile(PLAIN_TEXT);
    assert.equal(hex(await signWith(unlockedPlain.privateKey, PC_SIGNED)), hex(fromB64(fixture.handshake.pcSignature)));

    const rewrapped = await rewrapKeyFile(PLAIN_TEXT, undefined, "named pass");
    assert.notEqual(rewrapped.kdf, null);
    assert.equal(rewrapped.id, PLAIN.id);
    assert.equal((await unlockKeyFile(JSON.stringify(rewrapped), "named pass")).id, PLAIN.id);

    const created = await createKeyFile({ label: "Named", passphrase: "named pass" });
    assert.equal((await unlockKeyFile(JSON.stringify(created), "named pass")).id, created.id);
    assert.equal((await createKeyFile({ label: "Plain named" })).kdf, null);
    await expectError(createKeyFile({ label: "Empty", passphrase: "" }), "invalid-input", /must not be empty/);

    assert.ok(shim.constructed.length >= 4, `worker constructions: ${shim.constructed.length}`);
    assert.ok(shim.constructed.every(entry => entry.url.endsWith("/devicekey-kdf-worker.js")), JSON.stringify(shim.constructed.map(entry => entry.url)));
  } finally {
    restore();
  }
  assert.equal(typeof globalThis.Worker, workerBefore, "the shim is removed afterwards");
});

test("createDeviceKeyApi returns the full API and needs a function KDF", () => {
  assert.throws(() => createDeviceKeyApi({ kdf: "not a function" }), DeviceKeyError);
  const methods = createDeviceKeyApi({ kdf: argon2Kdf });
  assert.deepEqual(Object.keys(methods).sort(), ["canonicalId", "createKeyFile", "deriveId", "keyFileAAD", "parseKeyFile", "rewrapKeyFile", "unlockKeyFile"]);
  assert.equal(typeof createKeyFile, "function");
  assert.equal(typeof parseKeyFile, "function");
  assert.equal(typeof unlockKeyFile, "function");
  assert.equal(typeof rewrapKeyFile, "function");
  assert.equal(typeof deriveId, "function");
  assert.equal(typeof canonicalId, "function");
  assert.equal(typeof keyFileAAD, "function");
});

// A deliberately small asynchronous fake: requests succeed before transaction
// commit, and an injected commit failure aborts without persisting a write.
import { createDeviceKeyStore } from "../public/devicekey-store.js";
import { Worker as ThreadWorker } from "node:worker_threads";
import { deriveKeyInWorker } from "../public/devicekey-kdf-worker.js";

function memoryIDB() {
  const records = new Map();
  const state = { records, opens: 0, closes: 0, upgrades: 0, commits: 0, abortNext: false, blockedNext: false };
  let exists = false;
  const db = () => ({
    objectStoreNames: { contains: name => exists && name === "keys" },
    createObjectStore(name) { assert.equal(name, "keys"); exists = true; state.upgrades++; },
    close() { state.closes++; },
    transaction(name, mode) {
      assert.equal(name, "keys");
      assert.ok(mode === "readonly" || mode === "readwrite");
      const transaction = { error: null };
      const run = (kind, key, value) => {
        assert.equal(key, "device");
        const request = { result: undefined, error: null };
        queueMicrotask(() => {
          request.result = kind === "get" ? records.get(key) : kind === "put" ? key : undefined;
          request.onsuccess?.();
          queueMicrotask(() => {
            if (state.abortNext) {
              state.abortNext = false;
              transaction.error = new Error("simulated quota failure at commit");
              transaction.onabort?.();
            } else {
              if (kind === "put") records.set(key, value);
              if (kind === "delete") records.delete(key);
              state.commits++;
              transaction.oncomplete?.();
            }
          });
        });
        return request;
      };
      transaction.objectStore = name => {
        assert.equal(name, "keys");
        return {
          get: key => run("get", key),
          put: (value, key) => { assert.equal(mode, "readwrite"); return run("put", key, value); },
          delete: key => { assert.equal(mode, "readwrite"); return run("delete", key); },
        };
      };
      return transaction;
    },
  });
  return {
    state,
    open(name, version) {
      assert.equal(name, "bubbleplay-device");
      assert.equal(version, 1);
      state.opens++;
      const request = { result: db() };
      queueMicrotask(() => {
        if (state.blockedNext) { state.blockedNext = false; request.onblocked?.(); }
        if (!exists) request.onupgradeneeded?.();
        request.onsuccess?.();
      });
      return request;
    },
  };
}

test("IndexedDB stores the original JSON text in exactly one device record and clears it", async () => {
  const indexedDB = memoryIDB();
  const store = createDeviceKeyStore({ indexedDB });
  assert.equal(await store.load(), null);
  const original = "  " + JSON.stringify(fixture.keyFiles.passphrase.file, null, 2) + "\n";
  await store.save(original);
  assert.equal(await createDeviceKeyStore({ indexedDB }).load(), original);
  assert.deepEqual([...indexedDB.state.records], [["device", original]]);
  assert.equal(indexedDB.state.upgrades, 1);
  await store.clear();
  await store.clear();
  assert.equal(await store.load(), null);
  assert.equal(indexedDB.state.records.size, 0);
  assert.equal(indexedDB.state.opens, indexedDB.state.closes);
});

test("IndexedDB reports commit failures instead of reporting a successful request as saved", async () => {
  const indexedDB = memoryIDB();
  const store = createDeviceKeyStore({ indexedDB });
  await store.save("original JSON text");
  const committed = indexedDB.state.commits;
  indexedDB.state.abortNext = true;
  await assert.rejects(store.save("replacement JSON text"), /quota failure/);
  assert.equal(indexedDB.state.commits, committed);
  assert.equal(await store.load(), "original JSON text");
  indexedDB.state.abortNext = true;
  await assert.rejects(store.clear(), /quota failure/);
  assert.equal(await store.load(), "original JSON text");
  assert.equal(indexedDB.state.opens, indexedDB.state.closes);
});

test("IndexedDB handles unavailable storage, blocked opens and non-text records", async () => {
  await assert.rejects(createDeviceKeyStore({ indexedDB: null }).load(), /unavailable/);
  const indexedDB = memoryIDB();
  const store = createDeviceKeyStore({ indexedDB });
  await assert.rejects(store.save({ privateKey: "not text" }), /JSON text only/);
  assert.equal(indexedDB.state.opens, 0);
  indexedDB.state.blockedNext = true;
  await assert.rejects(store.load(), /blocked/);
  // A late successful open after onblocked must still close its connection.
  await Promise.resolve();
  assert.equal(indexedDB.state.opens, indexedDB.state.closes);
  indexedDB.state.records.set("device", new Uint8Array(32));
  await assert.rejects(store.load(), /not JSON text/);
  assert.equal(indexedDB.state.opens, indexedDB.state.closes);
});

// Node has WebCrypto but no browser Worker global. Adapt worker_threads only at
// the message boundary so this test executes the actual module Worker entrypoint
// and the unchanged vendored build in a separate thread, including transfers.
class ModuleWorkerBridge {
  constructor(url, options) {
    assert.equal(options.type, "module");
    const bootstrap = `
      import { parentPort } from 'node:worker_threads';
      globalThis.WorkerGlobalScope = class {
        static [Symbol.hasInstance](value) { return value === globalThis; }
      };
      globalThis.postMessage = (data, transfer) => parentPort.postMessage(data, transfer);
      await import(${JSON.stringify(url.href)});
      parentPort.on('message', data => globalThis.onmessage({ data }));
    `;
    this.thread = new ThreadWorker(new URL("data:text/javascript," + encodeURIComponent(bootstrap)));
    this.thread.on("message", data => this.onmessage?.({ data }));
    this.thread.on("error", () => this.onerror?.({ preventDefault() {} }));
    ModuleWorkerBridge.active++;
  }
  static active = 0;
  postMessage(data, transfer) { this.thread.postMessage(data, transfer); }
  terminate() { ModuleWorkerBridge.active--; return this.thread.terminate(); }
}

test("dedicated module Worker derives the fixture Argon2id key and terminates", async () => {
  const original = globalThis.Worker;
  globalThis.Worker = ModuleWorkerBridge;
  const vector = fixture.keyFiles.passphrase;
  const password = new TextEncoder().encode(vector.passphrase);
  const salt = new Uint8Array(Buffer.from(vector.file.kdf.salt, "base64url"));
  let ticked = false;
  const timer = setTimeout(() => { ticked = true; }, 0);
  let key;
  try {
    key = await deriveKeyInWorker({ password, salt });
    assert.equal(Buffer.from(key).toString("hex"), vector.kdfKeyHex);
    assert.equal(ticked, true, "main thread timers run during Argon2id");
    assert.equal(ModuleWorkerBridge.active, 0);
    assert.equal(new TextDecoder().decode(password), vector.passphrase, "caller retains input ownership");
    assert.equal(Buffer.from(salt).toString("base64url"), vector.file.kdf.salt);
  } finally {
    clearTimeout(timer);
    password.fill(0);
    salt.fill(0);
    key?.fill(0);
    if (original === undefined) delete globalThis.Worker; else globalThis.Worker = original;
  }
});

test("KDF Worker failures reject and terminate without exposing inputs", async () => {
  const original = globalThis.Worker;
  const password = new TextEncoder().encode("test-only password"), salt = new Uint8Array(16);
  let terminated = 0;
  try {
    for (const failure of ["load", "message", "response"]) {
      globalThis.Worker = class {
        postMessage() {
          queueMicrotask(() => {
            if (failure === "load") this.onerror({ preventDefault() {} });
            else if (failure === "message") this.onmessageerror();
            else this.onmessage({ data: { key: new Uint8Array(7) } });
          });
        }
        terminate() { terminated++; }
      };
      await assert.rejects(deriveKeyInWorker({ password, salt }), /Worker|Argon2id/);
    }
    assert.equal(terminated, 3);
    delete globalThis.Worker;
    await assert.rejects(deriveKeyInWorker({ password, salt }), /Worker support/);
  } finally {
    password.fill(0);
    salt.fill(0);
    if (original === undefined) delete globalThis.Worker; else globalThis.Worker = original;
  }
});
test("default exported API creates, parses, unlocks and rewraps protected JSON through Workers", async () => {
  const original = globalThis.Worker;
  globalThis.Worker = ModuleWorkerBridge;
  try {
    const file = await createKeyFile({ label: "Worker round trip", passphrase: "test-only worker password" });
    const text = JSON.stringify(file);
    assert.deepEqual(await parseKeyFile(text, "test-only worker password"), file);
    const unlocked = await unlockKeyFile(text, "test-only worker password");
    assert.equal(unlocked.id, file.id);
    assert.equal(unlocked.privateKey.extractable, false);
    assert.equal(await deriveId(unlocked.publicKey), file.id);
    const plain = await rewrapKeyFile(text, "test-only worker password");
    assert.equal(plain.kdf, null);
    assert.equal((await unlockKeyFile(JSON.stringify(plain))).id, file.id);
    assert.equal(ModuleWorkerBridge.active, 0);
  } finally {
    if (original === undefined) delete globalThis.Worker; else globalThis.Worker = original;
  }
});

test("KDF Worker rejects empty passwords and invalid salt sizes before starting a thread", async () => {
  await assert.rejects(deriveKeyInWorker({ password: new Uint8Array(), salt: new Uint8Array(16) }), /nonempty/);
  await assert.rejects(deriveKeyInWorker({ password: new Uint8Array([1]), salt: new Uint8Array(15) }), /16-byte salt/);
});