import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import * as crypto from "node:crypto";

// Deliberately independent of tools/device-keys/gen-contract.mjs.
const fixture = JSON.parse(readFileSync(new URL("../../fixtures/device-keys-contract.json", import.meta.url), "utf8"));
const fromHex = value => {
  assert.match(value, /^(?:[0-9a-f]{2})+$/);
  return Buffer.from(value, "hex");
};
const bytes = (value, length) => {
  assert.match(value, /^[A-Za-z0-9_-]+$/);
  const decoded = Buffer.from(value, "base64url");
  assert.equal(decoded.toString("base64url"), value, "canonical unpadded base64url");
  if (length !== undefined) assert.equal(decoded.length, length);
  return decoded;
};
const digest = value => crypto.createHash("sha256").update(value).digest();
const text = value => Buffer.from(value, "utf8");
// uint32 big-endian byte length before each field.
function lengthPrefixed(fields) {
  const out = Buffer.alloc(fields.reduce((total, field) => total + 4 + field.length, 0));
  let offset = 0;
  for (const field of fields) {
    out.writeUInt32BE(field.length, offset);
    field.copy(out, offset + 4);
    offset += 4 + field.length;
  }
  return out;
}
function secret(seedHex, curve = "ed25519") {
  const prefix = curve === "ed25519" ? "302e020100300506032b657004220420" : "302e020100300506032b656e04220420";
  const seed = fromHex(seedHex);
  assert.equal(seed.length, 32);
  return crypto.createPrivateKey({ key: Buffer.concat([fromHex(prefix), seed]), format: "der", type: "pkcs8" });
}
function rawPublic(privateKey) {
  return bytes(crypto.createPublicKey(privateKey).export({ format: "jwk" }).x, 32);
}
function publicKey(raw, curve = "ed25519") {
  return crypto.createPublicKey({ key: { kty: "OKP", crv: curve === "ed25519" ? "Ed25519" : "X25519", x: raw.toString("base64url") }, format: "jwk" });
}
function idOf(raw) {
  const alphabet = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";
  let pending = 0, bitCount = 0, encoded = "";
  for (const byte of digest(raw).subarray(0, 10)) {
    pending = (pending << 8) | byte;
    bitCount += 8;
    while (bitCount >= 5) {
      bitCount -= 5;
      encoded += alphabet[(pending >>> bitCount) & 31];
    }
    pending &= (1 << bitCount) - 1;
  }
  return [0, 4, 8, 12].map(start => encoded.slice(start, start + 4)).join("-");
}
function parseId(input) {
  const normalized = input.toUpperCase().replaceAll("-", "").replaceAll("O", "0").replace(/[IL]/g, "1");
  assert.match(normalized, /^[0-9ABCDEFGHJKMNPQRSTVWXYZ]{16}$/);
  return normalized.match(/.{4}/g).join("-");
}
// JCS serialization for the fixture's JSON domain (no lone surrogates/nonfinite numbers).
function canonical(value) {
  if (value === null || typeof value !== "object") return JSON.stringify(value);
  if (Array.isArray(value)) return "[" + value.map(canonical).join(",") + "]";
  const keys = Object.keys(value).sort((a, b) => a < b ? -1 : a > b ? 1 : 0);
  return "{" + keys.map(key => JSON.stringify(key) + ":" + canonical(value[key])).join(",") + "}";
}
function fileAAD(file) {
  const { private: omittedPrivate, aead, ...header } = file;
  if (aead === null) header.aead = null;
  else {
    const { nonce: omittedNonce, ...algorithm } = aead;
    header.aead = algorithm;
  }
  return text(canonical(header));
}
function keyFromPassphrase(file, passphrase) {
  return crypto.argon2Sync("argon2id", {
    message: text(passphrase), nonce: bytes(file.kdf.salt, 16),
    memory: file.kdf.m, passes: file.kdf.t, parallelism: file.kdf.p, tagLength: 32,
  });
}
function seal(key, nonce, aad, plaintext) {
  const cipher = crypto.createCipheriv("aes-256-gcm", key, nonce, { authTagLength: 16 });
  cipher.setAAD(aad);
  const encrypted = Buffer.concat([cipher.update(plaintext), cipher.final()]);
  return Buffer.concat([encrypted, cipher.getAuthTag()]);
}
function open(key, nonce, aad, encrypted) {
  const decipher = crypto.createDecipheriv("aes-256-gcm", key, nonce, { authTagLength: 16 });
  decipher.setAAD(aad);
  decipher.setAuthTag(encrypted.subarray(-16));
  return Buffer.concat([decipher.update(encrypted.subarray(0, -16)), decipher.final()]);
}
function validateIdentity(file, seed) {
  assert.equal(file.format, "bubbleplay-device-key");
  assert.equal(file.version, 1);
  assert.equal(file.type, "ed25519");
  const publicBytes = rawPublic(secret(seed.toString("hex")));
  assert.deepEqual(publicBytes, bytes(file.public, 32));
  assert.equal(idOf(publicBytes), file.id);
}

test("three ID derivations and lowercase, O/I/L, unhyphenated parsing", () => {
  assert.equal(fixture.version, 1);
  assert.match(fixture.notes, /docs\/architecture\/device-keys\.md/);
  assert.equal(fixture.ids.length, 3);
  for (const vector of fixture.ids) {
    const publicBytes = rawPublic(secret(vector.seedHex));
    assert.deepEqual(publicBytes, bytes(vector.public, 32));
    assert.equal(digest(publicBytes).toString("hex"), vector.sha256Hex);
    assert.equal(idOf(publicBytes), vector.id);
    assert.equal(parseId(vector.id), vector.id);
    if (vector.parse) {
      assert.match(vector.parse.input, /o/);
      assert.match(vector.parse.input, /i/);
      assert.match(vector.parse.input, /l/);
      assert.equal(vector.parse.input.includes("-"), false);
      assert.equal(parseId(vector.parse.input), vector.id);
      assert.equal(vector.parse.canonical, vector.id);
    }
  }
});

test("plain bpkey contains the seed and consistent public key/ID", () => {
  const { seedHex, file } = fixture.keyFiles.plain;
  assert.equal(file.kdf, null);
  assert.equal(file.aead, null);
  const seed = bytes(file.private, 32);
  assert.deepEqual(seed, fromHex(seedHex));
  validateIdentity(file, seed);
});

test("protected bpkey metadata and RFC 8785 AAD remain checkable on Node 22", () => {
  const vector = fixture.keyFiles.passphrase, file = vector.file;
  assert.deepEqual(Object.keys(file).sort(), ["format", "version", "type", "id", "label", "created", "public", "kdf", "aead", "private"].sort());
  assert.deepEqual(file.kdf, { alg: "argon2id", m: 65536, t: 3, p: 1, salt: file.kdf.salt });
  assert.deepEqual(file.aead, { alg: "aes-256-gcm", nonce: file.aead.nonce });
  bytes(file.kdf.salt, 16);
  bytes(file.aead.nonce, 12);
  bytes(file.private, 48);
  validateIdentity(file, fromHex(vector.seedHex));
  assert.equal(fileAAD(file).toString("utf8"), vector.aadUtf8);
  assert.equal(fileAAD(file).toString("hex"), vector.aadHex);
  // Sorting is independent of the order used by the serialized key file.
  assert.deepEqual(fileAAD(Object.fromEntries(Object.entries(file).reverse())), fileAAD(file));
  const [wrong, tampered] = fixture.keyFiles.failures;
  assert.deepEqual(fixture.keyFiles.failures.map(v => v.name), ["wrong-passphrase", "tampered-aad"]);
  assert.deepEqual(wrong.file, file);
  assert.notEqual(wrong.passphrase, vector.passphrase);
  assert.equal(tampered.passphrase, vector.passphrase);
  assert.deepEqual({ ...tampered.file, label: file.label }, file);
  assert.notDeepEqual(fileAAD(tampered.file), fileAAD(file));
});

test("Argon2id protected bpkey encrypts and decrypts the seed", t => {
  if (typeof crypto.argon2Sync !== "function") return t.skip("needs Node >= 24.7 for argon2");
  const { seedHex, passphrase, kdfKeyHex, file } = fixture.keyFiles.passphrase;
  const key = keyFromPassphrase(file, passphrase);
  assert.equal(key.toString("hex"), kdfKeyHex);
  const encrypted = seal(key, bytes(file.aead.nonce, 12), fileAAD(file), fromHex(seedHex));
  assert.deepEqual(encrypted, bytes(file.private, 48));
  const seed = open(key, bytes(file.aead.nonce, 12), fileAAD(file), encrypted);
  assert.deepEqual(seed, fromHex(seedHex));
  validateIdentity(file, seed);
});
for (const vector of fixture.keyFiles.failures) {
  test(`protected bpkey refuses ${vector.name}`, t => {
    if (typeof crypto.argon2Sync !== "function") return t.skip("needs Node >= 24.7 for argon2");
    assert.equal(vector.expected, "authentication-failed");
    const key = keyFromPassphrase(vector.file, vector.passphrase);
    assert.throws(() => open(key, bytes(vector.file.aead.nonce, 12), fileAAD(vector.file), bytes(vector.file.private, 48)), /authenticate/i);
  });
}

function handshakeInputs() {
  const h = fixture.handshake;
  assert.match(h.sessionEpoch, /^[0-9]+$/);
  for (const peer of [h.viewer, h.pc]) {
    const identityPublic = rawPublic(secret(peer.seedHex));
    assert.deepEqual(identityPublic, bytes(peer.public, 32));
    assert.equal(idOf(identityPublic), peer.id);
    assert.deepEqual(rawPublic(secret(peer.ephemeralPrivateHex, "x25519")), bytes(peer.ephemeral, 32));
  }
  const fields = [text("bubbleplay-auth-v1"), text(h.relayOrigin), text(h.sessionEpoch),
    text(h.viewer.id), bytes(h.viewer.public, 32), text(h.pc.id), bytes(h.pc.public, 32),
    bytes(h.viewer.nonce, 32), bytes(h.pc.nonce, 32), bytes(h.viewer.ephemeral, 32), bytes(h.pc.ephemeral, 32)];
  const transcript = Buffer.alloc(fields.reduce((total, field) => total + 4 + field.length, 0));
  let offset = 0;
  for (const field of fields) {
    transcript.writeUInt32BE(field.length, offset);
    field.copy(transcript, offset + 4);
    offset += 4 + field.length;
  }
  return { h, transcript, hash: digest(transcript) };
}
test("length-prefixed transcript and chained Ed25519 signatures", () => {
  const { h, transcript, hash } = handshakeInputs();
  assert.equal(transcript.toString("hex"), h.transcriptHex);
  assert.equal(hash.toString("hex"), h.transcriptHashHex);
  const hostBytes = Buffer.concat([text("bubbleplay-auth-host-v1"), hash]);
  const hostSignature = crypto.sign(null, hostBytes, secret(h.pc.seedHex));
  assert.equal(hostBytes.toString("hex"), h.pcSignedBytesHex);
  assert.deepEqual(hostSignature, bytes(h.pcSignature, 64));
  assert.equal(crypto.verify(null, hostBytes, publicKey(bytes(h.pc.public, 32)), hostSignature), true);
  const viewerBytes = Buffer.concat([text("bubbleplay-auth-viewer-v1"), hash, hostSignature]);
  const viewerSignature = crypto.sign(null, viewerBytes, secret(h.viewer.seedHex));
  assert.equal(viewerBytes.toString("hex"), h.viewerSignedBytesHex);
  assert.deepEqual(viewerSignature, bytes(h.viewerSignature, 64));
  assert.equal(crypto.verify(null, viewerBytes, publicKey(bytes(h.viewer.public, 32)), viewerSignature), true);
  const tampered = Buffer.from(viewerBytes);
  tampered[tampered.length - 1] ^= 1;
  assert.equal(crypto.verify(null, tampered, publicKey(bytes(h.viewer.public, 32)), viewerSignature), false);
});

test("X25519, independent HKDF expansion, and control GCM sequences 0/1 in both directions", () => {
  const { h, hash } = handshakeInputs();
  const viewerShared = crypto.diffieHellman({ privateKey: secret(h.viewer.ephemeralPrivateHex, "x25519"), publicKey: publicKey(bytes(h.pc.ephemeral, 32), "x25519") });
  const pcShared = crypto.diffieHellman({ privateKey: secret(h.pc.ephemeralPrivateHex, "x25519"), publicKey: publicKey(bytes(h.viewer.ephemeral, 32), "x25519") });
  assert.deepEqual(viewerShared, pcShared);
  assert.equal(viewerShared.toString("hex"), h.sharedSecretHex);
  // RFC 5869 extract + single expansion block for a 32-byte SHA-256 key.
  const prk = crypto.createHmac("sha256", hash).update(viewerShared).digest();
  const keys = {};
  for (const [direction, field] of [["viewer-to-pc", "viewerToPcHex"], ["pc-to-viewer", "pcToViewerHex"]]) {
    keys[direction] = crypto.createHmac("sha256", prk).update(text(`bubbleplay-ctl-v1 ${direction}`)).update(Buffer.from([1])).digest();
    assert.equal(keys[direction].toString("hex"), h.controlKeys[field]);
  }
  assert.notDeepEqual(keys["viewer-to-pc"], keys["pc-to-viewer"]);
  assert.deepEqual(h.controlMessages.map(v => [v.direction, v.sequence]).sort(), [["pc-to-viewer", 0], ["pc-to-viewer", 1], ["viewer-to-pc", 0], ["viewer-to-pc", 1]]);
  for (const vector of h.controlMessages) {
    const nonce = Buffer.alloc(12);
    nonce.writeBigUInt64BE(BigInt(vector.sequence), 4);
    assert.equal(nonce.toString("hex"), vector.nonceHex);
    assert.equal(JSON.parse(vector.plaintextUtf8).type, vector.type);
    const encrypted = seal(keys[vector.direction], nonce, text(vector.type), text(vector.plaintextUtf8));
    assert.deepEqual(encrypted, bytes(vector.ciphertext));
    assert.equal(open(keys[vector.direction], nonce, text(vector.type), encrypted).toString("utf8"), vector.plaintextUtf8);
    assert.throws(() => open(keys[vector.direction], nonce, text(vector.type + "-tampered"), encrypted), /authenticate/i);
    const wrongNonce = Buffer.from(nonce);
    wrongNonce[11] ^= 1;
    assert.throws(() => open(keys[vector.direction], wrongNonce, text(vector.type), encrypted), /authenticate/i);
  }
});

test("DTLS binding signatures preserve each side's local/remote order", () => {
  const { h, hash } = handshakeInputs();
  assert.deepEqual(Object.keys(h.dtls).sort(), ["pc", "viewer"]);
  assert.equal(h.dtls.viewer.localFingerprint, h.dtls.pc.remoteFingerprint);
  assert.equal(h.dtls.pc.localFingerprint, h.dtls.viewer.remoteFingerprint);
  for (const side of ["viewer", "pc"]) {
    const vector = h.dtls[side];
    for (const fingerprint of [vector.localFingerprint, vector.remoteFingerprint]) {
      assert.match(fingerprint, /^sha-256 (?:[0-9A-F]{2}:){31}[0-9A-F]{2}$/);
    }
    const signed = Buffer.concat([text("bubbleplay-dtls-v1"), hash, text(vector.localFingerprint + "\n" + vector.remoteFingerprint)]);
    assert.equal(signed.toString("hex"), vector.signedBytesHex);
    const signature = crypto.sign(null, signed, secret(h[side].seedHex));
    assert.deepEqual(signature, bytes(vector.signature, 64));
    assert.equal(crypto.verify(null, signed, publicKey(bytes(h[side].public, 32)), signature), true);
    const reversed = Buffer.concat([text("bubbleplay-dtls-v1"), hash, text(vector.remoteFingerprint + "\n" + vector.localFingerprint)]);
    assert.equal(crypto.verify(null, reversed, publicKey(bytes(h[side].public, 32)), signature), false);
  }
});

test("four signaling examples have exact shapes and match the handshake", () => {
  const h = fixture.handshake, messages = h.messages;
  assert.deepEqual(Object.keys(messages).sort(), ["authBind", "authBindPc", "authFinish", "authHello", "authHost"]);
  assert.deepEqual(messages.authHello, { type: "auth-hello", id: h.viewer.id, public: h.viewer.public,
    nonce: h.viewer.nonce, ephemeral: h.viewer.ephemeral, caps: ["control-aead-v1", "dtls-bind-v1"] });
  assert.deepEqual(messages.authHost, { type: "auth-host", id: h.pc.id, public: h.pc.public,
    nonce: h.pc.nonce, ephemeral: h.pc.ephemeral, signature: h.pcSignature });
  assert.deepEqual(messages.authFinish, { type: "auth-finish", signature: h.viewerSignature });
  assert.deepEqual(messages.authBind, { type: "auth-bind", signature: h.dtls.viewer.signature });
  assert.deepEqual(messages.authBindPc, { type: "auth-bind", signature: h.dtls.pc.signature });
  assert.equal(crypto.verify(null, fromHex(h.dtls.pc.signedBytesHex), publicKey(bytes(h.pc.public, 32)), bytes(messages.authBindPc.signature, 64)), true);
});

test("relay registration proofs recompute encodings, hashes and signatures", () => {
  const h = fixture.handshake, rp = fixture.relayProof;
  assert.deepEqual(Object.keys(rp).sort(), ["messages", "vectors"]);
  assert.deepEqual(rp.vectors.map(v => v.name), ["host", "viewer", "wrong-origin"]);
  const vectorNamed = name => rp.vectors.find(v => v.name === name);
  const identityOf = { host: "pc", viewer: "viewer", "wrong-origin": "pc" };
  const roleOf = { host: "host", viewer: "viewer", "wrong-origin": "host" };
  const expectedOf = { host: "valid", viewer: "valid", "wrong-origin": "invalid" };
  const checked = {};
  for (const vector of rp.vectors) {
    assert.deepEqual(Object.keys(vector).sort(), ["encodingHex", "expected", "id", "name", "nonce", "nonceHex", "origin", "public", "role", "sha256Hex", "signature"]);
    assert.equal(vector.role, roleOf[vector.name]);
    assert.equal(vector.expected, expectedOf[vector.name]);
    if (vector.expected === "valid") assert.equal(vector.origin, h.relayOrigin);
    else assert.notEqual(vector.origin, h.relayOrigin);
    const ident = fixture.ids.find(identity => identity.name === identityOf[vector.name]);
    assert.ok(ident, `identity ${identityOf[vector.name]} in fixture.ids`);
    const pub = rawPublic(secret(ident.seedHex));
    assert.deepEqual(bytes(vector.public, 32), pub);
    assert.equal(vector.id, idOf(pub));
    const nonce = bytes(vector.nonce, 32);
    assert.equal(nonce.length, 32);
    assert.deepEqual(nonce, fromHex(vector.nonceHex));
    const encoding = lengthPrefixed([text("bubbleplay-relay-v1"), text(vector.role), text(vector.origin), nonce, text(vector.id), pub]);
    assert.equal(encoding.toString("hex"), vector.encodingHex);
    const hash = digest(encoding);
    assert.equal(hash.toString("hex"), vector.sha256Hex);
    const signature = bytes(vector.signature, 64);
    if (vector.expected === "valid") {
      assert.deepEqual(crypto.sign(null, hash, secret(ident.seedHex)), signature);
      assert.equal(crypto.verify(null, hash, publicKey(pub), signature), true);
    } else {
      assert.equal(crypto.verify(null, hash, publicKey(pub), signature), false, "signature must not cover the changed origin");
    }
    checked[vector.name] = { pub, nonce, encoding, hash, signature };
  }
  const host = checked.host, wrong = checked["wrong-origin"];
  const hostVector = vectorNamed("host"), wrongVector = vectorNamed("wrong-origin");
  assert.equal(wrongVector.signature, hostVector.signature);
  for (const field of ["role", "nonce", "id", "public"]) assert.equal(wrongVector[field], hostVector[field], field);
  assert.notDeepEqual(wrong.encoding, host.encoding);
  // The host signature is valid for the original encoding and invalid once the origin is substituted.
  assert.equal(crypto.verify(null, host.hash, publicKey(host.pub), host.signature), true);
  assert.equal(crypto.verify(null, wrong.hash, publicKey(host.pub), host.signature), false);
  // Changing role, ID or nonce alone must break the host signature.
  const viewerId = fixture.ids.find(identity => identity.name === "viewer").id;
  const flippedNonce = Buffer.from(host.nonce);
  flippedNonce[0] ^= 1;
  const mutants = {
    role: lengthPrefixed([text("bubbleplay-relay-v1"), text("viewer"), text(h.relayOrigin), host.nonce, text(hostVector.id), host.pub]),
    id: lengthPrefixed([text("bubbleplay-relay-v1"), text("host"), text(h.relayOrigin), host.nonce, text(viewerId), host.pub]),
    nonce: lengthPrefixed([text("bubbleplay-relay-v1"), text("host"), text(h.relayOrigin), flippedNonce, text(hostVector.id), host.pub]),
  };
  for (const [name, encoding] of Object.entries(mutants)) {
    assert.equal(crypto.verify(null, digest(encoding), publicKey(host.pub), host.signature), false, `${name} mutation still verifies`);
  }
  const m = rp.messages;
  assert.deepEqual(Object.keys(m).sort(), ["hello", "helloOk", "registerChallenge", "registerProof"]);
  assert.deepEqual(m.hello, { type: "hello", role: "host", token: "test-only-registration-token", id: hostVector.id, public: hostVector.public });
  assert.deepEqual(m.registerChallenge, { type: "register-challenge", nonce: hostVector.nonce });
  assert.deepEqual(m.registerProof, { type: "register-proof", signature: hostVector.signature });
  assert.deepEqual(Object.keys(m.helloOk).sort(), ["ticket", "type"]);
  assert.equal(m.helloOk.type, "hello-ok");
  bytes(m.helloOk.ticket, 32);
});

// ---- Phase 2 (decisions 2026-10-25). Recomputed independently from the phase 0 handshake inputs.
const hkdf32 = (ikm, salt, info) => {
  const prk = crypto.createHmac("sha256", salt).update(ikm).digest();
  return crypto.createHmac("sha256", prk).update(info).update(Buffer.from([1])).digest();
};
const normaliseCode = input => input.toUpperCase().replace(/[-\s]/g, "").replaceAll("O", "0").replace(/[IL]/g, "1");

test("phase 2 pairing: code normalisation, relay admit hash, pair key, MACs and comparison number", () => {
  const { h, hash } = handshakeInputs();
  const p = fixture.phase2.pairing;
  assert.match(p.code, /^[0-9ABCDEFGHJKMNPQRSTVWXYZ]{10}$/);
  assert.equal(p.codeDisplay, `${p.code.slice(0, 5)}-${p.code.slice(5)}`);
  for (const v of p.normalise) assert.equal(normaliseCode(v.input), v.canonical);
  assert.equal(p.pcId, h.pc.id);
  const admitBytes = Buffer.concat([text("bubbleplay-pair-admit-v1"), text(h.pc.id), text(p.code)]);
  assert.equal(admitBytes.toString("hex"), p.admitBytesHex);
  assert.deepEqual(bytes(p.admit, 32), digest(admitBytes));
  const key = hkdf32(text(p.code), Buffer.alloc(0), text("bubbleplay-pair-v1"));
  assert.equal(key.toString("hex"), p.pairKeyHex);
  const viewerMac = crypto.createHmac("sha256", key).update(text("viewer")).update(hash).digest();
  const pcMac = crypto.createHmac("sha256", key).update(text("pc")).update(hash).digest();
  assert.deepEqual(bytes(p.viewerMac, 32), viewerMac);
  assert.deepEqual(bytes(p.pcMac, 32), pcMac);
  assert.notDeepEqual(viewerMac, pcMac);
  assert.equal(crypto.timingSafeEqual(viewerMac, bytes(p.viewerMac, 32)), true);
  assert.notEqual(p.wrongCode, p.code);
  assert.equal(crypto.timingSafeEqual(bytes(p.wrongCodeViewerMac, 32), viewerMac), false);
  const wrongKey = hkdf32(text(p.wrongCode), Buffer.alloc(0), text("bubbleplay-pair-v1"));
  assert.deepEqual(crypto.createHmac("sha256", wrongKey).update(text("viewer")).update(hash).digest(), bytes(p.wrongCodeViewerMac, 32));
  const sasBytes = Buffer.concat([text("bubbleplay-pair-sas-v1"), hash]);
  assert.equal(sasBytes.toString("hex"), p.sasBytesHex);
  const u32 = digest(sasBytes).readUInt32BE(0);
  assert.equal(u32, p.sasU32);
  assert.equal(p.sasNumber, String(u32 % 1000000).padStart(6, "0"));
  assert.match(p.sasNumber, /^[0-9]{6}$/);
  assert.equal(p.sasDisplay, `${p.sasNumber.slice(0, 3)} ${p.sasNumber.slice(3)}`);
  assert.equal(p.qr.android, `bubbleplay://pair?relay=${encodeURIComponent(h.relayOrigin)}&pc=${h.pc.id}&code=${p.code}`);
  assert.equal(p.qr.browser, `${h.relayOrigin}/pair#pc=${h.pc.id}&code=${p.code}`);
  const m = p.messages;
  assert.deepEqual(m.pairOpen, { type: "pair-open", admit: p.admit });
  assert.deepEqual(m.pairClose, { type: "pair-close" });
  assert.deepEqual(m.pairClosed, { type: "pair-closed" });
  assert.deepEqual(m.viewerHello, { type: "hello", role: "viewer", id: h.viewer.id, public: h.viewer.public, pair: p.admit });
  assert.deepEqual(m.authHello, { ...h.messages.authHello, caps: ["control-aead-v1", "dtls-bind-v1", "pair-v1"] });
  assert.deepEqual(m.pairProofViewer, { type: "pair-proof", mac: p.viewerMac });
  assert.deepEqual(m.pairProofPc, { type: "pair-proof", mac: p.pcMac });
  assert.deepEqual(m.pairDone, { type: "pair-done" });
  assert.deepEqual(m.pairDenied, { type: "pair-denied" });
});

test("phase 2 admit list and caps", () => {
  const { h } = handshakeInputs();
  assert.deepEqual(fixture.phase2.admit, { type: "admit", viewers: [h.viewer.id], remotePairing: false });
  assert.deepEqual(fixture.phase2.caps, { mandatory: ["control-aead-v1"], mandatoryWebRtc: ["control-aead-v1", "dtls-bind-v1"], pairing: "pair-v1" });
  // auth-bind keeps the phase 0 encoding (amendment 2026-10-25); checked by the DTLS test above.
  assert.equal(fixture.phase2.bind.encoding, "phase0");
});

function ctlKeys() {
  const { h, hash } = handshakeInputs();
  const shared = crypto.diffieHellman({ privateKey: secret(h.pc.ephemeralPrivateHex, "x25519"), publicKey: publicKey(bytes(h.viewer.ephemeral, 32), "x25519") });
  return {
    "viewer-to-pc": hkdf32(shared, hash, text("bubbleplay-ctl-v1 viewer-to-pc")),
    "pc-to-viewer": hkdf32(shared, hash, text("bubbleplay-ctl-v1 pc-to-viewer")),
  };
}
// Receiver per the decisions: exact next seq, AAD = kind, JSON plaintext.type must equal kind.
function receiveCtl(keys, direction, expectedSeq, message) {
  assert.deepEqual(Object.keys(message).sort(), ["data", "kind", "seq", "type"]);
  assert.equal(message.type, "ctl");
  assert.match(message.kind, /^[a-z0-9-]{1,32}$/);
  if (message.seq !== expectedSeq) throw new Error("ctl sequence mismatch");
  const nonce = Buffer.alloc(12);
  nonce.writeBigUInt64BE(BigInt(message.seq), 4);
  const plain = open(keys[direction], nonce, text(message.kind), bytes(message.data));
  if (message.kind !== "input") {
    if (JSON.parse(plain.toString("utf8")).type !== message.kind) throw new Error("ctl kind mismatch");
  }
  return plain;
}

test("phase 2 ctl envelopes: seq 0 and 1 each direction, JSON and binary input", () => {
  const keys = ctlKeys();
  const h = fixture.handshake;
  assert.equal(keys["viewer-to-pc"].toString("hex"), h.controlKeys.viewerToPcHex);
  assert.equal(keys["pc-to-viewer"].toString("hex"), h.controlKeys.pcToViewerHex);
  const msgs = fixture.phase2.ctl.messages;
  assert.deepEqual(msgs.map(v => [v.direction, v.seq]), [["viewer-to-pc", 0], ["viewer-to-pc", 1], ["pc-to-viewer", 0], ["pc-to-viewer", 1]]);
  assert.ok(msgs.some(v => v.encoding === "binary") && msgs.some(v => v.encoding === "json"));
  for (const v of msgs) {
    const nonce = Buffer.alloc(12);
    nonce.writeBigUInt64BE(BigInt(v.seq), 4);
    assert.equal(nonce.toString("hex"), v.nonceHex);
    assert.equal(v.aadUtf8, v.kind);
    assert.equal(v.message.kind, v.kind);
    assert.equal(v.message.seq, v.seq);
    const plain = fromHex(v.plaintextHex);
    if (v.encoding === "json") assert.equal(plain.toString("utf8"), v.plaintextUtf8);
    else { assert.equal(v.kind, "input"); assert.equal(v.direction, "viewer-to-pc"); assert.equal(v.plaintextUtf8, undefined); }
    assert.deepEqual(seal(keys[v.direction], nonce, text(v.kind), plain), bytes(v.message.data));
    assert.deepEqual(receiveCtl(keys, v.direction, v.seq, v.message), plain);
  }
  // The binary input frame is the input-contract "arrow-left-down" packet.
  const inputContract = JSON.parse(readFileSync(new URL("../../fixtures/input-contract.json", import.meta.url), "utf8"));
  const packet = JSON.stringify(inputContract.packets.find(p => p.name === "arrow-left-down").bytes);
  assert.equal(packet, JSON.stringify([...fromHex(msgs[0].plaintextHex)]));
});

for (const vector of fixture.phase2.ctl.failures) {
  test(`phase 2 ctl refuses ${vector.name}`, () => {
    assert.equal(vector.expected, "end-session");
    assert.throws(() => receiveCtl(ctlKeys(), vector.direction, vector.expectedSeq, vector.message), /authenticate|sequence|kind/i);
  });
}

test("phase 2 hex epoch handshake: opaque 32-hex epoch bytes in the transcript", () => {
  const { h } = handshakeInputs();
  const v = fixture.phase2.hexEpochHandshake;
  assert.match(v.sessionEpoch, /^[0-9a-f]{32}$/);
  const transcript = lengthPrefixed([text("bubbleplay-auth-v1"), text(h.relayOrigin), text(v.sessionEpoch),
    text(h.viewer.id), bytes(h.viewer.public, 32), text(h.pc.id), bytes(h.pc.public, 32),
    bytes(h.viewer.nonce, 32), bytes(h.pc.nonce, 32), bytes(h.viewer.ephemeral, 32), bytes(h.pc.ephemeral, 32)]);
  assert.equal(transcript.toString("hex"), v.transcriptHex);
  const hash = digest(transcript);
  assert.equal(hash.toString("hex"), v.transcriptHashHex);
  const hostBytes = Buffer.concat([text("bubbleplay-auth-host-v1"), hash]);
  assert.deepEqual(crypto.sign(null, hostBytes, secret(h.pc.seedHex)), bytes(v.pcSignature, 64));
  const viewerBytes = Buffer.concat([text("bubbleplay-auth-viewer-v1"), hash, bytes(v.pcSignature, 64)]);
  assert.deepEqual(crypto.sign(null, viewerBytes, secret(h.viewer.seedHex)), bytes(v.viewerSignature, 64));
  const shared = fromHex(h.sharedSecretHex);
  assert.equal(hkdf32(shared, hash, text("bubbleplay-ctl-v1 viewer-to-pc")).toString("hex"), v.controlKeys.viewerToPcHex);
  assert.equal(hkdf32(shared, hash, text("bubbleplay-ctl-v1 pc-to-viewer")).toString("hex"), v.controlKeys.pcToViewerHex);
  assert.notEqual(v.controlKeys.viewerToPcHex, h.controlKeys.viewerToPcHex);
  assert.equal(v.sasNumber, String(digest(Buffer.concat([text("bubbleplay-pair-sas-v1"), hash])).readUInt32BE(0) % 1000000).padStart(6, "0"));
  assert.deepEqual(v.peerJoined, { type: "peer-joined", mediaEpoch: v.sessionEpoch, pairing: false });
});
