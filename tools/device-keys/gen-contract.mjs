#!/usr/bin/env node
// TEST ONLY: every private key, salt and nonce below is public and deterministic.
// Never use these values for real identities, key files or sessions.
import * as crypto from "node:crypto";
import { writeFileSync } from "node:fs";

if (typeof crypto.argon2Sync !== "function") {
  console.error("Device-key contract generation requires crypto.argon2Sync (Node >= 24.7); no fixture was written.");
  process.exit(1);
}

const b64 = bytes => Buffer.from(bytes).toString("base64url");
const hex = value => Buffer.from(value, "hex");
const utf8 = value => Buffer.from(value, "utf8");
const sha256 = bytes => crypto.createHash("sha256").update(bytes).digest();
const alphabet = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";

// RFC 8785: ECMAScript primitive serialization, recursive UTF-16 property sorting.
// All fixture strings are valid Unicode and all numbers are finite integers.
function jcs(value) {
  if (Array.isArray(value)) return `[${value.map(jcs).join(",")}]`;
  if (value !== null && typeof value === "object") {
    return `{${Object.keys(value).sort().map(key => `${JSON.stringify(key)}:${jcs(value[key])}`).join(",")}}`;
  }
  return JSON.stringify(value);
}
function privateKey(seed, curve) {
  const prefix = curve === "ed25519" ? "302e020100300506032b657004220420" : "302e020100300506032b656e04220420";
  return crypto.createPrivateKey({ key: Buffer.concat([hex(prefix), seed]), format: "der", type: "pkcs8" });
}
function publicRaw(key) {
  return crypto.createPublicKey(key).export({ format: "der", type: "spki" }).subarray(-32);
}
function deviceId(publicKey) {
  let bits = BigInt(`0x${sha256(publicKey).subarray(0, 10).toString("hex")}`);
  let result = "";
  for (let i = 0; i < 16; i++) {
    result = alphabet[Number(bits & 31n)] + result;
    bits >>= 5n;
  }
  return result.match(/.{4}/g).join("-");
}
function aad(file) {
  const object = structuredClone(file);
  delete object.private;
  if (object.aead !== null) delete object.aead.nonce;
  return utf8(jcs(object));
}
function encrypt(key, nonce, associated, plain) {
  const cipher = crypto.createCipheriv("aes-256-gcm", key, nonce, { authTagLength: 16 });
  cipher.setAAD(associated);
  return Buffer.concat([cipher.update(plain), cipher.final(), cipher.getAuthTag()]);
}
function lengthPrefixed(fields) {
  return Buffer.concat(fields.flatMap(field => {
    const size = Buffer.alloc(4);
    size.writeUInt32BE(field.length);
    return [size, field];
  }));
}
function identity(name, seedHex) {
  const publicKey = publicRaw(privateKey(hex(seedHex), "ed25519"));
  return { name, seedHex, public: b64(publicKey), sha256Hex: sha256(publicKey).toString("hex"), id: deviceId(publicKey) };
}

// TEST ONLY: RFC 8032 seeds for the first two identities; fixed alias-test seed.
const ids = [
  identity("viewer", "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60"),
  identity("pc", "4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb"),
  identity("crockford-aliases", "0000000000000000000000000000000000000000000000000000000000000006"),
];
ids[2].parse = { input: "hnobw2w6oqqilm47", canonical: ids[2].id };

function keyFile(identity, label) {
  return {
    format: "bubbleplay-device-key", version: 1, type: "ed25519", id: identity.id,
    label, created: "2026-10-13T09:30:00Z", public: identity.public,
    kdf: null, aead: null, private: b64(hex(identity.seedHex)),
  };
}
const plainFile = keyFile(ids[1], "PC test");
const protectedFile = keyFile(ids[0], 'Viewer "test" café');
const passphrase = "correct horse battery staple";
protectedFile.kdf = { alg: "argon2id", m: 65536, t: 3, p: 1, salt: b64(hex("000102030405060708090a0b0c0d0e0f")) };
protectedFile.aead = { alg: "aes-256-gcm", nonce: b64(hex("101112131415161718191a1b")) };
const kdfKey = crypto.argon2Sync("argon2id", {
  message: utf8(passphrase), nonce: Buffer.from(protectedFile.kdf.salt, "base64url"),
  parallelism: 1, tagLength: 32, memory: 65536, passes: 3,
});
const associated = aad(protectedFile);
protectedFile.private = b64(encrypt(kdfKey, Buffer.from(protectedFile.aead.nonce, "base64url"), associated, hex(ids[0].seedHex)));
const tamperedFile = structuredClone(protectedFile);
tamperedFile.label = "Tampered viewer label";

function peer(identity, nonceHex, ephemeralPrivateHex) {
  return {
    seedHex: identity.seedHex, id: identity.id, public: identity.public,
    nonce: b64(hex(nonceHex)), ephemeralPrivateHex,
    ephemeral: b64(publicRaw(privateKey(hex(ephemeralPrivateHex), "x25519"))),
  };
}
const viewer = peer(ids[0], "202122232425262728292a2b2c2d2e2f303132333435363738393a3b3c3d3e3f",
  "404142434445464748494a4b4c4d4e4f505152535455565758595a5b5c5d5e5f");
const pc = peer(ids[1], "606162636465666768696a6b6c6d6e6f707172737475767778797a7b7c7d7e7f",
  "808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9f");
const relayOrigin = "https://example.test";
// Decimal UTF-8 string, deliberately beyond JS's safe integer range.
const sessionEpoch = "9007199254740993";
const transcript = lengthPrefixed([
  utf8("bubbleplay-auth-v1"), utf8(relayOrigin), utf8(sessionEpoch),
  utf8(viewer.id), Buffer.from(viewer.public, "base64url"), utf8(pc.id), Buffer.from(pc.public, "base64url"),
  Buffer.from(viewer.nonce, "base64url"), Buffer.from(pc.nonce, "base64url"),
  Buffer.from(viewer.ephemeral, "base64url"), Buffer.from(pc.ephemeral, "base64url"),
]);
const transcriptHash = sha256(transcript);
const pcSignedBytes = Buffer.concat([utf8("bubbleplay-auth-host-v1"), transcriptHash]);
const pcSignature = crypto.sign(null, pcSignedBytes, privateKey(hex(pc.seedHex), "ed25519"));
const viewerSignedBytes = Buffer.concat([utf8("bubbleplay-auth-viewer-v1"), transcriptHash, pcSignature]);
const viewerSignature = crypto.sign(null, viewerSignedBytes, privateKey(hex(viewer.seedHex), "ed25519"));
const sharedSecret = crypto.diffieHellman({
  privateKey: privateKey(hex(viewer.ephemeralPrivateHex), "x25519"),
  publicKey: crypto.createPublicKey(privateKey(hex(pc.ephemeralPrivateHex), "x25519")),
});
const controlKeys = {
  viewerToPcHex: Buffer.from(crypto.hkdfSync("sha256", sharedSecret, transcriptHash, utf8("bubbleplay-ctl-v1 viewer-to-pc"), 32)).toString("hex"),
  pcToViewerHex: Buffer.from(crypto.hkdfSync("sha256", sharedSecret, transcriptHash, utf8("bubbleplay-ctl-v1 pc-to-viewer"), 32)).toString("hex"),
};
const controlMessages = [];
for (const direction of ["viewer-to-pc", "pc-to-viewer"]) {
  for (const sequence of [0, 1]) {
    const type = direction === "viewer-to-pc" ? "input" : "clipboard";
    const plaintextUtf8 = direction === "viewer-to-pc"
      ? JSON.stringify({ type, action: "key", key: sequence === 0 ? "A" : "B" })
      : JSON.stringify({ type, text: sequence === 0 ? "hello" : "world" });
    const nonce = Buffer.alloc(12);
    nonce.writeBigUInt64BE(BigInt(sequence), 4);
    const key = hex(direction === "viewer-to-pc" ? controlKeys.viewerToPcHex : controlKeys.pcToViewerHex);
    controlMessages.push({ direction, sequence, type, plaintextUtf8, nonceHex: nonce.toString("hex"),
      ciphertext: b64(encrypt(key, nonce, utf8(type), utf8(plaintextUtf8))) });
  }
}
const fingerprint = start => "sha-256 " + Array.from({ length: 32 }, (_, i) => (start + i).toString(16).padStart(2, "0").toUpperCase()).join(":");
const viewerFingerprint = fingerprint(0xa0);
const pcFingerprint = fingerprint(0xc0);
function binding(peer, localFingerprint, remoteFingerprint) {
  const signedBytes = Buffer.concat([utf8("bubbleplay-dtls-v1"), transcriptHash,
    utf8(localFingerprint), utf8("\n"), utf8(remoteFingerprint)]);
  return { localFingerprint, remoteFingerprint, signedBytesHex: signedBytes.toString("hex"),
    signature: b64(crypto.sign(null, signedBytes, privateKey(hex(peer.seedHex), "ed25519"))) };
}
const dtls = { viewer: binding(viewer, viewerFingerprint, pcFingerprint), pc: binding(pc, pcFingerprint, viewerFingerprint) };

function relayProofEncoding(vector) {
  return lengthPrefixed([
    utf8("bubbleplay-relay-v1"), utf8(vector.role), utf8(vector.origin),
    Buffer.from(vector.nonce, "base64url"), utf8(vector.id), Buffer.from(vector.public, "base64url"),
  ]);
}
function relayProofVector(name, role, identity, nonceHex) {
  const vector = { name, role, origin: relayOrigin, nonceHex, nonce: b64(hex(nonceHex)),
    id: identity.id, public: identity.public };
  const encoding = relayProofEncoding(vector);
  const hash = sha256(encoding);
  return { ...vector, encodingHex: encoding.toString("hex"), sha256Hex: hash.toString("hex"),
    signature: b64(crypto.sign(null, hash, privateKey(hex(identity.seedHex), "ed25519"))), expected: "valid" };
}
const relayHost = relayProofVector("host", "host", ids[1], "a0a1a2a3a4a5a6a7a8a9aaabacadaeafb0b1b2b3b4b5b6b7b8b9babbbcbdbebf");
const relayViewer = relayProofVector("viewer", "viewer", ids[0], "c0c1c2c3c4c5c6c7c8c9cacbcccdcecfd0d1d2d3d4d5d6d7d8d9dadbdcdddedf");
// Keep the valid host signature while substituting a different relay origin.
const relayWrongOrigin = { ...relayHost, name: "wrong-origin", origin: "https://other-relay.example.test", expected: "invalid" };
const wrongOriginEncoding = relayProofEncoding(relayWrongOrigin);
relayWrongOrigin.encodingHex = wrongOriginEncoding.toString("hex");
relayWrongOrigin.sha256Hex = sha256(wrongOriginEncoding).toString("hex");
// ---- Phase 2 (decisions 2026-10-25, items 2, 4, 5, 6, 8). Reuses the phase 0 handshake transcript hash H.
const hmac = (key, ...parts) => { const m = crypto.createHmac("sha256", key); for (const p of parts) m.update(p); return m.digest(); };
const normaliseCode = input => input.toUpperCase().replace(/[-\s]/g, "").replaceAll("O", "0").replace(/[IL]/g, "1");
// TEST ONLY: a fixed pairing code; real codes come from the PC's CSPRNG.
const pairCode = "70Q1K9MXR1";
const pairCodeInput = "7oq-ik9 mxrl";
if (normaliseCode(pairCodeInput) !== pairCode || !/^[0-9ABCDEFGHJKMNPQRSTVWXYZ]{10}$/.test(pairCode)) throw new Error("pair code vector");
const pairCodeDisplay = `${pairCode.slice(0, 5)}-${pairCode.slice(5)}`;
const admitBytes = Buffer.concat([utf8("bubbleplay-pair-admit-v1"), utf8(pc.id), utf8(pairCode)]);
const admitHash = sha256(admitBytes);
const pairKey = Buffer.from(crypto.hkdfSync("sha256", utf8(pairCode), Buffer.alloc(0), utf8("bubbleplay-pair-v1"), 32));
const viewerMac = hmac(pairKey, utf8("viewer"), transcriptHash);
const pcMac = hmac(pairKey, utf8("pc"), transcriptHash);
const wrongCode = "70Q1K9MXR2";
const wrongPairKey = Buffer.from(crypto.hkdfSync("sha256", utf8(wrongCode), Buffer.alloc(0), utf8("bubbleplay-pair-v1"), 32));
const sasBytes = Buffer.concat([utf8("bubbleplay-pair-sas-v1"), transcriptHash]);
const sasHash = sha256(sasBytes);
const sasU32 = sasHash.readUInt32BE(0);
const sasNumber = String(sasU32 % 1000000).padStart(6, "0");
const sasDisplay = `${sasNumber.slice(0, 3)} ${sasNumber.slice(3)}`;

// ctl envelopes: one direction each way, seq 0 and 1, JSON and binary (binary only viewer-to-pc, the WSS input frame).
const inputFrame = Buffer.from([13, 2, 75, 0, 37, 0]); // fixtures/input-contract.json packet "arrow-left-down"
const ctlPlain = [
  { direction: "viewer-to-pc", seq: 0, kind: "input", encoding: "binary", plaintext: inputFrame },
  { direction: "viewer-to-pc", seq: 1, kind: "file-cancel", encoding: "json", plaintext: utf8(JSON.stringify({ type: "file-cancel", id: "t1" })) },
  { direction: "pc-to-viewer", seq: 0, kind: "clipboard", encoding: "json", plaintext: utf8(JSON.stringify({ type: "clipboard", protocol: 1, text: "hello" })) },
  { direction: "pc-to-viewer", seq: 1, kind: "cursor", encoding: "json", plaintext: utf8(JSON.stringify({ type: "cursor", shape: "text" })) },
];
const ctl = ctlPlain.map(v => {
  const nonce = Buffer.alloc(12);
  nonce.writeBigUInt64BE(BigInt(v.seq), 4);
  const key = hex(v.direction === "viewer-to-pc" ? controlKeys.viewerToPcHex : controlKeys.pcToViewerHex);
  const data = b64(encrypt(key, nonce, utf8(v.kind), v.plaintext));
  return { direction: v.direction, seq: v.seq, kind: v.kind, encoding: v.encoding,
    plaintextHex: v.plaintext.toString("hex"), ...(v.encoding === "json" ? { plaintextUtf8: v.plaintext.toString("utf8") } : {}),
    nonceHex: nonce.toString("hex"), aadUtf8: v.kind, message: { type: "ctl", kind: v.kind, seq: v.seq, data } };
});
const ctlFirst = ctl[0];
const ctlFailures = [
  { name: "replayed-seq", direction: "viewer-to-pc", expectedSeq: 1, message: ctlFirst.message, expected: "end-session" },
  { name: "skipped-seq", direction: "viewer-to-pc", expectedSeq: 0, message: ctl[1].message, expected: "end-session" },
  { name: "wrong-kind-aad", direction: "viewer-to-pc", expectedSeq: 0, message: { ...ctlFirst.message, kind: "clipboard" }, expected: "end-session" },
  { name: "wrong-direction-key", direction: "pc-to-viewer", expectedSeq: 0, message: ctlFirst.message, expected: "end-session" },
];

// auth-bind (item 8, amended 2026-10-25): the phase 0 encoding wins, so phase 2 reuses handshake.dtls
// ("bubbleplay-dtls-v1" || H || local fp || "\n" || remote fp) and handshake.messages.authBind / authBindPc.

// Decision 22 (2026-10-27): the relay epoch is an opaque ASCII string; relay-rs uses 32 lowercase hex.
// Same peers, nonces and ephemeral keys as the phase 0 handshake, only the epoch differs.
const hexEpoch = "0f1e2d3c4b5a69788796a5b4c3d2e1f0";
const hexTranscript = lengthPrefixed([
  utf8("bubbleplay-auth-v1"), utf8(relayOrigin), utf8(hexEpoch),
  utf8(viewer.id), Buffer.from(viewer.public, "base64url"), utf8(pc.id), Buffer.from(pc.public, "base64url"),
  Buffer.from(viewer.nonce, "base64url"), Buffer.from(pc.nonce, "base64url"),
  Buffer.from(viewer.ephemeral, "base64url"), Buffer.from(pc.ephemeral, "base64url"),
]);
const hexHash = sha256(hexTranscript);
const hexPcSignature = crypto.sign(null, Buffer.concat([utf8("bubbleplay-auth-host-v1"), hexHash]), privateKey(hex(pc.seedHex), "ed25519"));
const hexViewerSignature = crypto.sign(null, Buffer.concat([utf8("bubbleplay-auth-viewer-v1"), hexHash, hexPcSignature]), privateKey(hex(viewer.seedHex), "ed25519"));
const hexEpochHandshake = {
  sessionEpoch: hexEpoch,
  transcriptHex: hexTranscript.toString("hex"), transcriptHashHex: hexHash.toString("hex"),
  pcSignature: b64(hexPcSignature), viewerSignature: b64(hexViewerSignature),
  controlKeys: {
    viewerToPcHex: Buffer.from(crypto.hkdfSync("sha256", sharedSecret, hexHash, utf8("bubbleplay-ctl-v1 viewer-to-pc"), 32)).toString("hex"),
    pcToViewerHex: Buffer.from(crypto.hkdfSync("sha256", sharedSecret, hexHash, utf8("bubbleplay-ctl-v1 pc-to-viewer"), 32)).toString("hex"),
  },
  sasNumber: String(sha256(Buffer.concat([utf8("bubbleplay-pair-sas-v1"), hexHash])).readUInt32BE(0) % 1000000).padStart(6, "0"),
  peerJoined: { type: "peer-joined", mediaEpoch: hexEpoch, pairing: false },
};

const phase2 = {
  notes: "Phase 2 vectors (decisions 2026-10-25). H is handshake.transcriptHashHex; control keys are handshake.controlKeys; auth-bind uses the phase 0 handshake.dtls encoding unchanged. Where phase 0 defines an encoding it wins. TEST ONLY.",
  pairing: {
    code: pairCode, codeDisplay: pairCodeDisplay,
    normalise: [{ input: pairCodeInput, canonical: pairCode }, { input: pairCodeDisplay, canonical: pairCode }],
    pcId: pc.id,
    admitBytesHex: admitBytes.toString("hex"), admit: b64(admitHash),
    pairKeyHex: pairKey.toString("hex"),
    viewerMac: b64(viewerMac), pcMac: b64(pcMac),
    wrongCode, wrongCodeViewerMac: b64(hmac(wrongPairKey, utf8("viewer"), transcriptHash)),
    sasBytesHex: sasBytes.toString("hex"), sasU32, sasNumber, sasDisplay,
    qr: {
      android: `bubbleplay://pair?relay=${encodeURIComponent(relayOrigin)}&pc=${pc.id}&code=${pairCode}`,
      browser: `${relayOrigin}/pair#pc=${pc.id}&code=${pairCode}`,
    },
    messages: {
      pairOpen: { type: "pair-open", admit: b64(admitHash) },
      pairClose: { type: "pair-close" },
      pairClosed: { type: "pair-closed" },
      viewerHello: { type: "hello", role: "viewer", id: viewer.id, public: viewer.public, pair: b64(admitHash) },
      authHello: { type: "auth-hello", id: viewer.id, public: viewer.public, nonce: viewer.nonce, ephemeral: viewer.ephemeral, caps: ["control-aead-v1", "dtls-bind-v1", "pair-v1"] },
      pairProofViewer: { type: "pair-proof", mac: b64(viewerMac) },
      pairProofPc: { type: "pair-proof", mac: b64(pcMac) },
      pairDone: { type: "pair-done" },
      pairDenied: { type: "pair-denied" },
    },
  },
  admit: { type: "admit", viewers: [viewer.id], remotePairing: false },
  ctl: { messages: ctl, failures: ctlFailures },
  bind: { encoding: "phase0", vectors: "handshake.dtls", messages: "handshake.messages.authBind, handshake.messages.authBindPc" },
  caps: { mandatory: ["control-aead-v1"], mandatoryWebRtc: ["control-aead-v1", "dtls-bind-v1"], pairing: "pair-v1" },
  hexEpochHandshake,
};

const fixture = {
  version: 1,
  notes: "See docs/architecture/device-keys.md, Phase 0 results. TEST ONLY: all private keys, passphrases, salts and nonces are public deterministic vectors; never use them in production. Binary JSON fields use unpadded base64url unless named Hex; plaintextUtf8 and aadUtf8 are exact UTF-8 strings.",
  ids,
  keyFiles: {
    plain: { seedHex: ids[1].seedHex, file: plainFile },
    passphrase: { seedHex: ids[0].seedHex, passphrase, kdfKeyHex: kdfKey.toString("hex"),
      aadUtf8: associated.toString("utf8"), aadHex: associated.toString("hex"), file: protectedFile },
    failures: [
      { name: "wrong-passphrase", passphrase: "incorrect horse battery staple", file: protectedFile, expected: "authentication-failed" },
      { name: "tampered-aad", passphrase, file: tamperedFile, expected: "authentication-failed" },
    ],
  },
  handshake: {
    relayOrigin, sessionEpoch, viewer, pc,
    transcriptHex: transcript.toString("hex"), transcriptHashHex: transcriptHash.toString("hex"),
    pcSignedBytesHex: pcSignedBytes.toString("hex"), pcSignature: b64(pcSignature),
    viewerSignedBytesHex: viewerSignedBytes.toString("hex"), viewerSignature: b64(viewerSignature),
    sharedSecretHex: sharedSecret.toString("hex"), controlKeys, controlMessages, dtls,
    messages: {
      authHello: { type: "auth-hello", id: viewer.id, public: viewer.public, nonce: viewer.nonce, ephemeral: viewer.ephemeral, caps: ["control-aead-v1", "dtls-bind-v1"] },
      authHost: { type: "auth-host", id: pc.id, public: pc.public, nonce: pc.nonce, ephemeral: pc.ephemeral, signature: b64(pcSignature) },
      authFinish: { type: "auth-finish", signature: b64(viewerSignature) },
      authBind: { type: "auth-bind", signature: dtls.viewer.signature },
      authBindPc: { type: "auth-bind", signature: dtls.pc.signature },
    },
  },
  relayProof: {
    vectors: [relayHost, relayViewer, relayWrongOrigin],
    messages: {
      hello: { type: "hello", role: "host", token: "test-only-registration-token", id: relayHost.id, public: relayHost.public },
      registerChallenge: { type: "register-challenge", nonce: relayHost.nonce },
      registerProof: { type: "register-proof", signature: relayHost.signature },
      // TEST ONLY: a deterministic stand-in for the relay's random ticket.
      helloOk: { type: "hello-ok", ticket: b64(hex("e0e1e2e3e4e5e6e7e8e9eaebecedeeeff0f1f2f3f4f5f6f7f8f9fafbfcfdfeff")) },
    },
  },
  phase2,
};
writeFileSync(new URL("../../fixtures/device-keys-contract.json", import.meta.url), JSON.stringify(fixture, null, 2) + "\n", "utf8");
console.log("Wrote fixtures/device-keys-contract.json");
