// Vector tests for worker/public/dk-crypto.js. Every expected value comes from
// fixtures/device-keys-contract.json (handshake = phase 0, phase2 = phase 2) or
// fixtures/input-contract.json. The fixture keys are public test vectors, never production keys.
//
// TEST ONLY helpers below wrap the fixture seeds in PKCS#8 and import them extractable, so the
// tests can read public halves back as raw bytes. The module's own TEST ONLY injections are
// newViewerHello({nonce, ephemeralSeed}), deriveCtlKeys(..., {extractable}) and pairKey(..., {extractable}).
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import * as dk from "../public/dk-crypto.js";
import { canonicalId, deriveId } from "../public/devicekey.js";

const readFixture = name => JSON.parse(readFileSync(new URL(`../../fixtures/${name}`, import.meta.url), "utf8"));
const contract = readFixture("device-keys-contract.json");
const inputContract = readFixture("input-contract.json");
const { handshake: hs, phase2: p2 } = contract;

const subtle = globalThis.crypto.subtle;
const hexBytes = text => new Uint8Array(Buffer.from(text, "hex"));
const b64uBytes = text => new Uint8Array(Buffer.from(text, "base64url"));
const b64u = bytes => Buffer.from(bytes).toString("base64url");
const toHex = bytes => Buffer.from(bytes).toString("hex");
const utf8 = text => new TextEncoder().encode(text);
const rawOf = async key => new Uint8Array(await subtle.exportKey("raw", key));
const flipFirstBit = bytes => {
  const copy = new Uint8Array(bytes);
  copy[0] ^= 1;
  return copy;
};
const tamper = text => b64u(flipFirstBit(b64uBytes(text)));

// TEST ONLY: PKCS#8 wrappers (RFC 8410) around the 32-byte fixture seeds.
const ED25519_PKCS8 = "302e020100300506032b657004220420";
const X25519_PKCS8 = "302e020100300506032b656e04220420";

async function ed25519(seedHex) {
  const privateKey = await subtle.importKey("pkcs8", hexBytes(ED25519_PKCS8 + seedHex), { name: "Ed25519" }, true, ["sign"]);
  const jwk = await subtle.exportKey("jwk", privateKey);
  return { privateKey, publicRaw: b64uBytes(jwk.x) };
}

async function x25519Private(seedHex) {
  return subtle.importKey("pkcs8", hexBytes(X25519_PKCS8 + seedHex), { name: "X25519" }, false, ["deriveBits"]);
}

// Fixture values in raw form.
const { relayOrigin, sessionEpoch: epoch } = hs;
const viewerId = hs.viewer.id;
const pcId = hs.pc.id;
const viewerPublic = b64uBytes(hs.viewer.public);
const pcPublic = b64uBytes(hs.pc.public);
const viewerNonce = b64uBytes(hs.viewer.nonce);
const pcNonce = b64uBytes(hs.pc.nonce);
const viewerEphemeral = b64uBytes(hs.viewer.ephemeral);
const pcEphemeral = b64uBytes(hs.pc.ephemeral);
const H = hexBytes(hs.transcriptHashHex);
const { dtls, messages } = hs;
const authHost = messages.authHost;
const hostArgs = { relayOrigin, epoch, viewerId, viewerPublic, viewerNonce, viewerEphemeral };
const pair = p2.pairing;
const caps = p2.caps;
const failures = Object.fromEntries(p2.ctl.failures.map(f => [f.name, f]));
const phase2Ctl = p2.ctl.messages;
const phase0Ctl = hs.controlMessages;
const inputPacket = inputContract.packets.find(p => p.name === "arrow-left-down").bytes;

// Both sides' control channels from the vector scalars. The viewer's ephemeral key comes from
// newViewerHello's TEST ONLY seed; the PC's is imported here. Each call makes fresh channels.
async function sessions() {
  const hello = await dk.newViewerHello({
    caps: caps.mandatoryWebRtc,
    nonce: viewerNonce,
    ephemeralSeed: hexBytes(hs.viewer.ephemeralPrivateHex),
  });
  const viewer = await dk.deriveCtlKeys(hello.ephemeralKeyPair.privateKey, pcEphemeral, H, { extractable: true });
  const pc = await dk.deriveCtlKeys(await x25519Private(hs.pc.ephemeralPrivateHex), viewerEphemeral, H, { extractable: true });
  return {
    hello,
    viewer,
    pc,
    viewerChannel: dk.createCtlChannel({ sendKey: viewer.send, recvKey: viewer.recv }),
    pcChannel: dk.createCtlChannel({ sendKey: pc.recv, recvKey: pc.send }),
  };
}

test("b64u encodes, decodes and refuses non-canonical text", () => {
  assert.equal(dk.b64u.encode(new Uint8Array([0xfb, 0xff])), "-_8");
  assert.deepEqual(dk.b64u.decode("-_8"), new Uint8Array([0xfb, 0xff]));
  assert.equal(dk.b64u.encode(new Uint8Array(0)), "");
  for (let n = 0; n < 40; n++) {
    const bytes = Uint8Array.from({ length: n }, (_, i) => (i * 37 + 11) & 255);
    assert.equal(dk.b64u.encode(bytes), b64u(bytes));
    assert.deepEqual(dk.b64u.decode(dk.b64u.encode(bytes)), bytes);
  }
  for (const bad of ["AA==", "+/8", "AB", "A", "-_9", "-_8 ", 12]) {
    assert.throws(() => dk.b64u.decode(bad), RangeError, JSON.stringify(bad));
  }
});

test("normalizePairCode reads typed and displayed codes as the canonical form", () => {
  for (const { input, canonical } of pair.normalise) {
    assert.equal(dk.normalizePairCode(input), canonical);
  }
  assert.equal(dk.normalizePairCode(pair.codeDisplay), pair.code);
  assert.equal(dk.normalizePairCode(pair.code.toLowerCase()), pair.code);
  assert.equal(dk.normalizePairCode("oIlO-12 3456"), "0110123456");
});

test("normalizePairCode refuses anything that is not 10 Crockford characters", () => {
  for (const bad of ["70Q1K9MXR", "70Q1K9MXR10", "70Q1K9MXRU", "70Q1K9MX!1", "", null]) {
    assert.throws(() => dk.normalizePairCode(bad), RangeError, JSON.stringify(bad));
  }
});

test("fixture seeds produce the fixture public keys, and the fixture IDs derive from them", async () => {
  assert.deepEqual((await ed25519(hs.viewer.seedHex)).publicRaw, viewerPublic);
  assert.deepEqual((await ed25519(hs.pc.seedHex)).publicRaw, pcPublic);
  for (const id of contract.ids) {
    assert.equal(await deriveId(b64uBytes(id.public)), id.id, id.name);
    if (id.parse) assert.equal(canonicalId(id.parse.input), id.parse.canonical, id.name);
  }
});

test("transcript bytes and hash match the handshake vector", async () => {
  const result = await dk.transcript({
    relayOrigin, epoch, viewerId, viewerPublic, pcId, pcPublic,
    viewerNonce, pcNonce, viewerEphemeral, pcEphemeral,
  });
  assert.equal(toHex(result.bytes), hs.transcriptHex);
  assert.equal(toHex(result.hash), hs.transcriptHashHex);
});

test("transcript refuses a numeric epoch, non-canonical IDs and wrong lengths", async () => {
  const fields = { relayOrigin, epoch, viewerId, viewerPublic, pcId, pcPublic, viewerNonce, pcNonce, viewerEphemeral, pcEphemeral };
  await assert.rejects(dk.transcript({ ...fields, epoch: 9007199254740993 }), TypeError);
  await assert.rejects(dk.transcript({ ...fields, viewerId: "47z33qx1ajh62rkb" }), TypeError);
  await assert.rejects(dk.transcript({ ...fields, viewerNonce: viewerNonce.subarray(1) }), TypeError);
});

test("transcript encodes a 32-lowercase-hex epoch as its exact bytes with length prefix 32", async () => {
  const mediaEpoch = "00abcdef0123456789abcdef01234567";
  const { bytes } = await dk.transcript({
    relayOrigin, epoch: mediaEpoch, viewerId, viewerPublic, pcId, pcPublic,
    viewerNonce, pcNonce, viewerEphemeral, pcEphemeral,
  });
  const offset = 4 + utf8("bubbleplay-auth-v1").length + 4 + utf8(relayOrigin).length;
  assert.deepEqual(bytes.subarray(offset, offset + 4), new Uint8Array([0, 0, 0, 32]));
  assert.deepEqual(bytes.subarray(offset + 4, offset + 36), utf8(mediaEpoch));
});

test("transcript rejects empty, non-ASCII, nonprintable and oversized epochs", async () => {
  const fields = { relayOrigin, epoch, viewerId, viewerPublic, pcId, pcPublic, viewerNonce, pcNonce, viewerEphemeral, pcEphemeral };
  for (const invalid of ["", "caf\u00e9", "abc\n", "\x7f", "a".repeat(65)]) {
    await assert.rejects(dk.transcript({ ...fields, epoch: invalid }), TypeError);
  }
  await dk.transcript({ ...fields, epoch: "a".repeat(64) });
});

test("pairAdmitHash, pairKey and the pair MACs match the pairing vectors", async () => {
  assert.equal(await dk.pairAdmitHash(pcId, pair.code), pair.admit);
  assert.equal(await dk.pairAdmitHash("77vh7m568gjky12j", pair.codeDisplay), pair.admit);

  const exported = await dk.pairKey(pair.codeDisplay, { extractable: true });
  assert.equal(toHex(await rawOf(exported)), pair.pairKeyHex);

  const key = await dk.pairKey(pair.code);
  assert.equal(key.extractable, false);
  assert.equal(await dk.pairMac(key, "viewer", H), pair.viewerMac);
  assert.equal(await dk.pairMac(key, "pc", H), pair.pcMac);
  assert.equal(await dk.verifyPairMac(key, "viewer", H, pair.viewerMac), true);
  assert.equal(await dk.verifyPairMac(key, "pc", H, pair.pcMac), true);
  assert.equal(await dk.verifyPairMac(key, "pc", H, pair.viewerMac), false, "a MAC from the other role");
  assert.equal(await dk.verifyPairMac(key, "pc", H, pair.pcMac.slice(0, -2)), false, "a MAC of the wrong length");
  assert.equal(await dk.verifyPairMac(key, "pc", H, "not a mac!"), false, "a MAC that is not base64url");
  await assert.rejects(dk.pairMac(key, "PC", H), TypeError);
});

test("a wrong code gives a viewer MAC the PC refuses", async () => {
  const wrong = await dk.pairKey(pair.wrongCode);
  assert.equal(await dk.pairMac(wrong, "viewer", H), pair.wrongCodeViewerMac);
  assert.equal(await dk.verifyPairMac(wrong, "viewer", H, pair.viewerMac), false);
});

test("comparisonNumber gives the six digits both screens show", async () => {
  assert.equal(await dk.comparisonNumber(H), pair.sasNumber);
});

test("newViewerHello and buildAuthHello produce the phase 0 and pairing auth-hello messages", async () => {
  const hello = await dk.newViewerHello({
    caps: caps.mandatoryWebRtc,
    nonce: viewerNonce,
    ephemeralSeed: hexBytes(hs.viewer.ephemeralPrivateHex),
  });
  assert.equal(b64u(hello.ephemeralPublic), hs.viewer.ephemeral);
  assert.deepEqual(
    dk.buildAuthHello({ id: viewerId, publicRaw: viewerPublic, nonce: hello.nonce, ephemeralPublic: hello.ephemeralPublic, caps: hello.caps }),
    messages.authHello,
  );

  const pairingCaps = [...caps.mandatoryWebRtc, caps.pairing];
  const pairingHello = await dk.newViewerHello({
    caps: pairingCaps,
    nonce: viewerNonce,
    ephemeralSeed: hexBytes(hs.viewer.ephemeralPrivateHex),
  });
  assert.deepEqual(
    dk.buildAuthHello({ id: viewerId, publicRaw: viewerPublic, nonce: pairingHello.nonce, ephemeralPublic: pairingHello.ephemeralPublic, caps: pairingCaps }),
    pair.messages.authHello,
  );
  assert.throws(() => dk.buildAuthHello({ id: viewerId, publicRaw: viewerPublic, nonce: viewerNonce, ephemeralPublic: viewerEphemeral, caps: "control-aead-v1" }), TypeError);
});

test("newViewerHello without the TEST ONLY seed draws a fresh nonce and a non-extractable key", async () => {
  const a = await dk.newViewerHello({ caps: caps.mandatoryWebRtc });
  const b = await dk.newViewerHello({ caps: caps.mandatoryWebRtc });
  assert.equal(a.nonce.length, 32);
  assert.notDeepEqual(a.nonce, b.nonce);
  assert.notDeepEqual(a.ephemeralPublic, b.ephemeralPublic);
  assert.equal(a.ephemeralKeyPair.privateKey.extractable, false);
  await assert.rejects(dk.newViewerHello({ caps: ["Pair V1"] }), TypeError);
});

test("verifyAuthHost accepts the PC's auth-host under the pinned key and returns H", async () => {
  const pinned = await dk.verifyAuthHost({ msg: authHost, pinnedPublic: pcPublic, ...hostArgs });
  assert.equal(pinned.ok, true);
  assert.equal(pinned.reason, null);
  assert.equal(toHex(pinned.H), hs.transcriptHashHex);
  assert.equal(toHex(pinned.pcPublic), toHex(pcPublic));

  const unpinned = await dk.verifyAuthHost({ msg: authHost, ...hostArgs });
  assert.equal(unpinned.ok, true);
});

test("verifyAuthHost refuses a changed PC key", async () => {
  const result = await dk.verifyAuthHost({ msg: authHost, pinnedPublic: viewerPublic, ...hostArgs });
  assert.deepEqual(result, { ok: false, reason: "pc-key-changed", H: null, pcPublic: null });
});

test("verifyAuthHost refuses bad signatures and bad shapes", async () => {
  const cases = [
    ["bad-signature", { ...authHost, signature: tamper(authHost.signature) }, {}],
    ["bad-signature", authHost, { relayOrigin: "https://other-relay.example.test" }],
    ["bad-signature", authHost, { viewerNonce: flipFirstBit(viewerNonce) }],
    ["bad-shape", { ...authHost, extra: 1 }, {}],
    ["bad-shape", { ...authHost, signature: authHost.signature.slice(0, -2) }, {}],
    ["bad-shape", { ...authHost, id: viewerId }, {}],
    ["bad-shape", { ...authHost, public: "not base64" }, {}],
    ["bad-shape", null, {}],
  ];
  for (const [reason, msg, override] of cases) {
    const result = await dk.verifyAuthHost({ msg, pinnedPublic: pcPublic, ...hostArgs, ...override });
    assert.deepEqual(result, { ok: false, reason, H: null, pcPublic: null }, reason);
  }
});

test("signAuthFinish reproduces the viewer's auth-finish signature", async () => {
  const viewer = await ed25519(hs.viewer.seedHex);
  assert.equal(await dk.signAuthFinish(viewer.privateKey, H, authHost.signature), messages.authFinish.signature);
});

test("deriveCtlKeys gives the control keys from both sides", async () => {
  const { viewer, pc } = await sessions();
  assert.equal(toHex(await rawOf(viewer.send)), hs.controlKeys.viewerToPcHex);
  assert.equal(toHex(await rawOf(viewer.recv)), hs.controlKeys.pcToViewerHex);
  assert.equal(toHex(await rawOf(pc.send)), hs.controlKeys.viewerToPcHex);
  assert.equal(toHex(await rawOf(pc.recv)), hs.controlKeys.pcToViewerHex);
});

test("production control keys are non-extractable", async () => {
  const hello = await dk.newViewerHello({ caps: caps.mandatoryWebRtc });
  const keys = await dk.deriveCtlKeys(hello.ephemeralKeyPair.privateKey, pcEphemeral, H);
  assert.equal(keys.send.extractable, false);
  assert.equal(keys.recv.extractable, false);
});

test("phase 0 control messages seal to the fixture ciphertext", async () => {
  const { viewerChannel, pcChannel } = await sessions();
  for (const m of phase0Ctl) {
    const channel = m.direction === "viewer-to-pc" ? viewerChannel : pcChannel;
    assert.equal(JSON.stringify(JSON.parse(m.plaintextUtf8)), m.plaintextUtf8, "fixture plaintext is canonical JSON");
    const sealed = await channel.sealJson(JSON.parse(m.plaintextUtf8));
    assert.deepEqual(sealed, { type: "ctl", kind: m.type, seq: m.sequence, data: m.ciphertext });
  }
});

test("the viewer seals the phase 2 viewer-to-pc vectors byte for byte", async () => {
  const { viewerChannel } = await sessions();
  const [input, cancel] = phase2Ctl.filter(m => m.direction === "viewer-to-pc");
  assert.deepEqual(await viewerChannel.seal("input", hexBytes(input.plaintextHex)), input.message);
  assert.deepEqual(await viewerChannel.sealJson(JSON.parse(cancel.plaintextUtf8)), cancel.message);
});

test("the PC seals the phase 2 pc-to-viewer vectors byte for byte", async () => {
  const { pcChannel } = await sessions();
  const [clipboard, cursor] = phase2Ctl.filter(m => m.direction === "pc-to-viewer");
  assert.deepEqual(await pcChannel.sealJson(JSON.parse(clipboard.plaintextUtf8)), clipboard.message);
  assert.deepEqual(await pcChannel.sealJson(JSON.parse(cursor.plaintextUtf8)), cursor.message);
});

test("the PC opens the viewer's vectors: binary input matches input-contract, JSON file-cancel parses", async () => {
  const { pcChannel } = await sessions();
  const [input, cancel] = phase2Ctl.filter(m => m.direction === "viewer-to-pc");
  assert.deepEqual(await pcChannel.open(input.message), Uint8Array.from(inputPacket));
  assert.deepEqual(await pcChannel.openJson(cancel.message), JSON.parse(cancel.plaintextUtf8));
});

test("the viewer opens the PC's phase 2 vectors", async () => {
  const { viewerChannel } = await sessions();
  const [clipboard, cursor] = phase2Ctl.filter(m => m.direction === "pc-to-viewer");
  assert.deepEqual(await viewerChannel.openJson(clipboard.message), JSON.parse(clipboard.plaintextUtf8));
  assert.deepEqual(await viewerChannel.openJson(cursor.message), JSON.parse(cursor.plaintextUtf8));
});

test("phase 0 control envelopes open to the fixture plaintext in both directions", async () => {
  const { viewerChannel, pcChannel } = await sessions();
  for (const m of phase0Ctl) {
    const channel = m.direction === "viewer-to-pc" ? pcChannel : viewerChannel;
    const envelope = { type: "ctl", kind: m.type, seq: m.sequence, data: m.ciphertext };
    assert.deepEqual(await channel.openJson(envelope), JSON.parse(m.plaintextUtf8));
  }
});

test("a replayed control message ends the channel", async () => {
  const { pcChannel } = await sessions();
  const [input, cancel] = phase2Ctl.filter(m => m.direction === "viewer-to-pc");
  assert.deepEqual(await pcChannel.open(input.message), Uint8Array.from(inputPacket));
  await assert.rejects(pcChannel.open(failures["replayed-seq"].message), dk.CtlError);
  await assert.rejects(pcChannel.openJson(cancel.message), dk.CtlError, "the next correct message is refused too");
});

test("a skipped control sequence ends the channel", async () => {
  const { pcChannel } = await sessions();
  const [input] = phase2Ctl.filter(m => m.direction === "viewer-to-pc");
  await assert.rejects(pcChannel.open(failures["skipped-seq"].message), dk.CtlError);
  await assert.rejects(pcChannel.open(input.message), dk.CtlError);
});

test("a wrong kind (AAD) ends the channel", async () => {
  const { pcChannel } = await sessions();
  const [input] = phase2Ctl.filter(m => m.direction === "viewer-to-pc");
  await assert.rejects(pcChannel.open(failures["wrong-kind-aad"].message), dk.CtlError);
  await assert.rejects(pcChannel.open(input.message), dk.CtlError);
});

test("a message under the wrong direction key ends the channel", async () => {
  const { viewerChannel } = await sessions();
  const [clipboard] = phase2Ctl.filter(m => m.direction === "pc-to-viewer");
  await assert.rejects(viewerChannel.open(failures["wrong-direction-key"].message), dk.CtlError);
  await assert.rejects(viewerChannel.openJson(clipboard.message), dk.CtlError);
});

test("a tampered ciphertext ends the channel", async () => {
  const { pcChannel } = await sessions();
  const [input] = phase2Ctl.filter(m => m.direction === "viewer-to-pc");
  const tampered = { ...input.message, data: b64u(flipFirstBit(b64uBytes(input.message.data))) };
  await assert.rejects(pcChannel.open(tampered), dk.CtlError);
  await assert.rejects(pcChannel.open(input.message), dk.CtlError);
});

test("malformed envelopes end the channel with CtlError", async () => {
  const bad = [
    { type: "x", kind: "input", seq: 0, data: "" },
    { type: "ctl", kind: "Bad Kind", seq: 0, data: "" },
    { type: "ctl", kind: "input", seq: "0", data: "" },
    { type: "ctl", kind: "input", seq: 0, data: "***" },
    { type: "ctl", kind: "input", seq: 0, data: "", extra: 1 },
    null,
  ];
  for (const envelope of bad) {
    const { pcChannel } = await sessions();
    await assert.rejects(pcChannel.open(envelope), dk.CtlError, JSON.stringify(envelope));
  }
});

test("openJson refuses a payload whose type differs from its kind, or that is not JSON", async () => {
  const { pcChannel, viewerChannel } = await sessions();
  const mismatched = await pcChannel.seal("clipboard", utf8(JSON.stringify({ type: "cursor", shape: "text" })));
  await assert.rejects(viewerChannel.openJson(mismatched), dk.CtlError);

  const session = await sessions();
  const notJson = await session.pcChannel.seal("clipboard", utf8("not json"));
  await assert.rejects(session.viewerChannel.openJson(notJson), dk.CtlError);
});

test("sealing refuses an invalid kind without ending the channel", async () => {
  const { pcChannel } = await sessions();
  await assert.rejects(pcChannel.seal("Bad Kind", new Uint8Array(0)), TypeError);
  const [clipboard] = phase2Ctl.filter(m => m.direction === "pc-to-viewer");
  assert.deepEqual(await pcChannel.sealJson(JSON.parse(clipboard.plaintextUtf8)), clipboard.message);
});

test("DTLS bind signatures reproduce for both sides and verify only in the signer's order", async () => {
  const viewer = await ed25519(hs.viewer.seedHex);
  const pc = await ed25519(hs.pc.seedHex);
  assert.equal(await dk.signBind(viewer.privateKey, H, dtls.viewer.localFingerprint, dtls.viewer.remoteFingerprint), dtls.viewer.signature);
  assert.equal(await dk.signBind(pc.privateKey, H, dtls.pc.localFingerprint, dtls.pc.remoteFingerprint), dtls.pc.signature);
  assert.equal(await dk.verifyBind(viewerPublic, H, dtls.viewer.localFingerprint, dtls.viewer.remoteFingerprint, dtls.viewer.signature), true);
  assert.equal(await dk.verifyBind(pcPublic, H, dtls.pc.localFingerprint, dtls.pc.remoteFingerprint, dtls.pc.signature), true);
  assert.equal(await dk.verifyBind(viewerPublic, H, dtls.viewer.remoteFingerprint, dtls.viewer.localFingerprint, dtls.viewer.signature), false);
  assert.equal(await dk.verifyBind(pcPublic, H, dtls.pc.localFingerprint, dtls.pc.remoteFingerprint, dtls.viewer.signature), false);
  assert.equal(await dk.verifyBind(viewerPublic, H, dtls.viewer.localFingerprint, dtls.viewer.remoteFingerprint, "not-a-signature"), false);
  await assert.rejects(dk.signBind(viewer.privateKey, H, `${dtls.viewer.localFingerprint}\n`, dtls.viewer.remoteFingerprint), TypeError);
});

test("parseSdpFingerprint reads one a=fingerprint value and refuses anything else", () => {
  const line = `a=fingerprint:${dtls.viewer.localFingerprint}`;
  assert.equal(dk.parseSdpFingerprint(`v=0\r\n${line}\r\na=setup:actpass\r\n${line}\r\n`), dtls.viewer.localFingerprint);
  assert.equal(dk.parseSdpFingerprint(`v=0\n${line}\n`), dtls.viewer.localFingerprint);
  assert.throws(() => dk.parseSdpFingerprint("v=0\r\n"), RangeError);
  assert.throws(() => dk.parseSdpFingerprint(`${line}\r\na=fingerprint:sha-256 00:11\r\n`), RangeError);
  assert.throws(() => dk.parseSdpFingerprint("a=fingerprint:sha-256 ZZ:11\r\n"), RangeError);
});
