// TEST ONLY: deterministic public fixture keys/nonces drive the real socket adapter and crypto.
import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createHash, createHmac, createPrivateKey, hkdfSync, sign } from "node:crypto";
import { createApp } from "../public/dk-app.js";
import { beginViewerHandshake } from "../public/dk-handshake.js";
import { openPairingSocket } from "../public/dk-relay.js";
import { normalizePcRecord } from "../public/devicekey-store.js";

const fixture = JSON.parse(readFileSync(new URL("../../fixtures/device-keys-contract.json", import.meta.url), "utf8"));
const hs = fixture.handshake;
const pair = fixture.phase2.pairing;
const bytes = text => new Uint8Array(Buffer.from(text, "base64url"));
const hex = text => new Uint8Array(Buffer.from(text, "hex"));
const CLOSED = "The PC closed the pairing window. Open Pair device on the PC again.";

async function harness({ code = pair.code, existing = null, mediaEpoch = hs.sessionEpoch } = {}) {
  const events = [];
  const sockets = [];
  class Socket {
    constructor(url) { this.url = url; this.sent = []; this.closed = false; sockets.push(this); }
    send(text) { assert.equal(this.closed, false); const message = JSON.parse(text); this.sent.push(message); events.push(["send", message.type]); }
    close() { if (this.closed) return; this.closed = true; events.push(["close"]); this.onclose?.({ code: 1000 }); }
    async receive(message) { await this.onmessage({ data: JSON.stringify(message) }); }
    remoteClose(code) { this.closed = true; return this.onclose({ code }); }
  }
  const privateKey = await crypto.subtle.importKey("pkcs8", hex("302e020100300506032b657004220420" + hs.viewer.seedHex),
    { name: "Ed25519" }, false, ["sign"]);
  const identity = { id: hs.viewer.id, publicKey: bytes(hs.viewer.public), privateKey };
  const screens = [];
  const gate = { screens, last: type => screens.filter(row => row[0] === type).at(-1)?.[1] };
  for (const type of ["showKeyScreen", "showPcList", "showPairing", "showRefusal", "showBusy", "hide"])
    gate[type] = options => { screens.push([type, options]); events.push([type, options?.state]); };
  const records = new Map(existing ? [[existing.id, existing]] : []);
  const store = {
    async load() { return JSON.stringify({ id: hs.viewer.id, label: "Viewer", kdf: null, aead: null }); },
    async listPcs() { return [...records.values()]; },
    async getPc(id) { return records.get(id) ?? null; },
    async putPc(record) { events.push(["save"]); records.set(record.id, normalizePcRecord(record)); },
  };
  const timers = new Map();
  let timerId = 0;
  const sessions = [];
  const location = { href: `${hs.relayOrigin}/pair#pc=${hs.pc.id}&code=${code}`, reload() { events.push(["reload"]); } };
  const history = { replaceState(_state, _title, url) { location.href = new URL(url, location.href).href; } };
  const handshakeCalls = [];
  const app = createApp({ gate, store, location, history,
    unlock: async () => identity,
    openPairing: options => openPairingSocket({ ...options, WebSocketImpl: Socket }),
    beginHandshake: options => {
      handshakeCalls.push(options);
      return beginViewerHandshake({ ...options, helloOptions: {
        nonce: bytes(hs.viewer.nonce), ephemeralSeed: hex(hs.viewer.ephemeralPrivateHex),
      } });
    },
    setTimer(callback, ms) { const id = ++timerId; timers.set(id, { callback, ms }); return id; },
    clearTimer(id) { timers.delete(id); },
    now: () => 123456,
    loadSession: async () => ({ async startSession(options) { events.push(["session"]); sessions.push(options); } }),
  });
  await app.start();
  const socket = sockets[0];
  assert.equal(new URL(location.href).hash, "");
  const admit = async (overrides = {}) => {
    socket.onopen();
    const challenge = fixture.relayProof.vectors.find(vector => vector.name === "viewer");
    await socket.receive({ type: "register-challenge", nonce: challenge.nonce });
    await socket.receive({ type: "hello-ok", pc: hs.pc.id, mediaEpoch, pairing: true, ...overrides });
  };
  const auth = async () => { await admit(); await socket.receive(hs.messages.authHost); };
  const compare = async () => { await auth(); await socket.receive(pair.messages.pairProofPc); };
  return { app, gate, records, socket, sockets, sessions, events, timers, admit, auth, compare, handshakeCalls };
}

test("pairing fixture: signed admission, auth-finish, both pair proofs, comparison, approval, save and normal connect", async () => {
  const h = await harness();
  await h.admit();
  assert.deepEqual(h.socket.sent[0], pair.messages.viewerHello);
  assert.equal(h.socket.sent[1].type, "register-proof");
  assert.deepEqual(h.socket.sent[2], pair.messages.authHello);
  assert.equal(h.handshakeCalls[0].pinnedPublic, null);
  assert.equal(h.handshakeCalls[0].epoch, "9007199254740993");
  await h.socket.receive(hs.messages.authHost);
  assert.deepEqual(h.socket.sent[3], hs.messages.authFinish);
  assert.deepEqual(h.socket.sent[4], pair.messages.pairProofViewer);
  assert.equal(h.gate.last("showPairing").state, "connecting");
  assert.equal(h.records.size, 0);
  await h.socket.receive(pair.messages.pairProofPc);
  assert.equal(h.gate.last("showPairing").state, "compare");
  assert.equal(h.gate.last("showPairing").number, pair.sasNumber);
  assert.equal(h.records.size, 0);
  await h.socket.receive(pair.messages.pairDone);
  const done = h.gate.last("showPairing");
  assert.equal(done.state, "done");
  assert.equal(h.records.size, 0);
  assert.equal(h.socket.closed, false);
  await done.onSave("  Office PC  ");
  assert.deepEqual(h.records.get(hs.pc.id), { id: hs.pc.id, public: hs.pc.public, name: "Office PC", paired: 123456, lastUsed: null });
  assert.equal(h.sessions.length, 1);
  assert.equal(h.sessions[0].pc, hs.pc.id);
  assert.equal(h.sessions[0].pinnedPublic, hs.pc.public);
  assert.deepEqual(h.events.filter(row => ["close", "save", "session"].includes(row[0])), [["save"], ["close"], ["session"]]);
  assert.equal(h.timers.size, 0);
  await done.onSave("Again");
  assert.equal(h.sessions.length, 1);
});

test("explicit pairing uses an unpinned auth-host even for a previously saved PC", async () => {
  const h = await harness({ existing: { id: hs.pc.id, public: hs.viewer.public, name: "Old", paired: 1, lastUsed: null } });
  await h.compare();
  assert.equal(h.handshakeCalls[0].pinnedPublic, null);
  assert.equal(h.gate.last("showPairing").number, pair.sasNumber);
  // Existing pin is not replaced before approval and Save.
  assert.equal(h.records.get(hs.pc.id).public, hs.viewer.public);
});

test("real hexadecimal mediaEpoch pairs end to end using an independently signed transcript", async () => {
  const mediaEpoch = "0123456789abcdef0123456789abcdef";
  const h = await harness({ mediaEpoch });
  await h.admit();
  assert.equal(h.handshakeCalls[0].epoch, mediaEpoch);
  // Recompute the exact protocol bytes with Node primitives, without the browser transcript,
  // signing, pair-key or MAC helpers. Leading zeroes remain part of the signed epoch text.
  const fields = [
    Buffer.from("bubbleplay-auth-v1"), Buffer.from(hs.relayOrigin), Buffer.from(mediaEpoch),
    Buffer.from(hs.viewer.id), Buffer.from(hs.viewer.public, "base64url"),
    Buffer.from(hs.pc.id), Buffer.from(hs.pc.public, "base64url"),
    Buffer.from(hs.viewer.nonce, "base64url"), Buffer.from(hs.pc.nonce, "base64url"),
    Buffer.from(hs.viewer.ephemeral, "base64url"), Buffer.from(hs.pc.ephemeral, "base64url"),
  ];
  const transcript = Buffer.concat(fields.flatMap(field => {
    const size = Buffer.alloc(4);
    size.writeUInt32BE(field.length);
    return [size, field];
  }));
  const H = createHash("sha256").update(transcript).digest();
  const edKey = seed => createPrivateKey({
    key: Buffer.from("302e020100300506032b657004220420" + seed, "hex"), type: "pkcs8", format: "der",
  });
  const pcSignature = sign(null, Buffer.concat([Buffer.from("bubbleplay-auth-host-v1"), H]), edKey(hs.pc.seedHex));
  await h.socket.receive({ ...hs.messages.authHost, signature: pcSignature.toString("base64url") });
  const viewerSignature = sign(null,
    Buffer.concat([Buffer.from("bubbleplay-auth-viewer-v1"), H, pcSignature]), edKey(hs.viewer.seedHex));
  assert.deepEqual(h.socket.sent[3], { type: "auth-finish", signature: viewerSignature.toString("base64url") });
  const K = Buffer.from(hkdfSync("sha256", Buffer.from(pair.code, "ascii"), Buffer.alloc(0), Buffer.from("bubbleplay-pair-v1"), 32));
  const mac = role => createHmac("sha256", K).update(Buffer.concat([Buffer.from(role), H])).digest("base64url");
  assert.deepEqual(h.socket.sent[4], { type: "pair-proof", mac: mac("viewer") });
  await h.socket.receive({ type: "pair-proof", mac: mac("pc") });
  const sasHash = createHash("sha256").update(Buffer.concat([Buffer.from("bubbleplay-pair-sas-v1"), H])).digest();
  const sas = String(sasHash.readUInt32BE(0) % 1000000).padStart(6, "0");
  assert.equal(h.gate.last("showPairing").number, sas);
  await h.socket.receive(pair.messages.pairDone);
  assert.equal(h.gate.last("showPairing").state, "done");
  await h.gate.last("showPairing").onSave("Hex PC");
  assert.equal(h.records.get(hs.pc.id).public, hs.pc.public);
  assert.equal(h.sessions.length, 1);
});

test("a socket close after pair-done preserves naming and can still save and connect", async () => {
  const h = await harness();
  await h.compare();
  await h.socket.receive(pair.messages.pairDone);
  await h.socket.remoteClose(1000);
  assert.equal(h.gate.last("showPairing").state, "done");
  await h.gate.last("showPairing").onSave("Desk");
  assert.equal(h.records.get(hs.pc.id).public, hs.pc.public);
  assert.equal(h.sessions.length, 1);
});

test("wrong code or wrong-role PC MAC fails without displaying a number or saving", async () => {
  for (const [code, proof] of [
    [pair.wrongCode, pair.messages.pairProofPc],
    [pair.code, pair.messages.pairProofViewer],
    [pair.code, { type: "pair-proof", mac: "invalid" }],
  ]) {
    const h = await harness({ code });
    await h.auth();
    await h.socket.receive(proof);
    assert.equal(h.gate.last("showPairing").state, "failed");
    assert.equal(h.gate.last("showPairing").message, "The pairing code did not match.");
    assert.equal(h.gate.screens.some(row => row[1]?.number), false);
    assert.equal(h.records.size, 0);
    assert.equal(h.socket.closed, true);
  }
});

test("forged auth-host or a different target ID never sends auth-finish or a pair proof", async () => {
  for (const override of [{ signature: hs.viewerSignature }, { id: hs.viewer.id }, { public: hs.viewer.public }]) {
    const h = await harness();
    await h.admit();
    await h.socket.receive({ ...hs.messages.authHost, ...override });
    assert.equal(h.gate.last("showPairing").state, "failed");
    assert.deepEqual(h.socket.sent.map(message => message.type), ["hello", "register-proof", "auth-hello"]);
    assert.equal(h.gate.screens.some(row => row[1]?.number), false);
    assert.equal(h.records.size, 0);
  }
});

test("pair-done before PC proof cannot save or show the comparison number", async () => {
  const h = await harness();
  await h.auth();
  await h.socket.receive(pair.messages.pairDone);
  assert.equal(h.gate.last("showPairing").state, "failed");
  assert.equal(h.gate.screens.some(row => row[1]?.number), false);
  assert.equal(h.records.size, 0);
});

test("back-to-back handshake, PC proof and pair-done remain ordered across WebCrypto awaits", async () => {
  const h = await harness();
  await h.admit();
  await Promise.all([
    h.socket.receive(hs.messages.authHost),
    h.socket.receive(pair.messages.pairProofPc),
    h.socket.receive(pair.messages.pairDone),
  ]);
  assert.deepEqual(h.socket.sent.slice(-2), [hs.messages.authFinish, pair.messages.pairProofViewer]);
  assert.deepEqual(h.gate.screens.filter(row => row[0] === "showPairing").map(row => row[1].state), ["connecting", "compare", "done"]);
});

test("PC proof and pair-done queued immediately before socket close preserve approval", async () => {
  const h = await harness();
  await h.auth();
  // Deliberately deliver close before the prior WebCrypto MAC verification has completed.
  const proof = h.socket.receive(pair.messages.pairProofPc);
  const done = h.socket.receive(pair.messages.pairDone);
  const close = h.socket.remoteClose(1000);
  await Promise.all([proof, done, close]);
  assert.deepEqual(h.gate.screens.filter(row => row[0] === "showPairing").map(row => row[1].state),
    ["connecting", "compare", "done"]);
  await h.gate.last("showPairing").onSave("Office");
  assert.equal(h.records.get(hs.pc.id).public, hs.pc.public);
  assert.equal(h.sessions.length, 1);
});

test("PC proof and pair-denied queued immediately before socket close preserve denial", async () => {
  const h = await harness();
  await h.auth();
  const proof = h.socket.receive(pair.messages.pairProofPc);
  const denied = h.socket.receive(pair.messages.pairDenied);
  const close = h.socket.remoteClose(1000);
  await Promise.all([proof, denied, close]);
  assert.equal(h.gate.last("showPairing").state, "denied");
  assert.equal(h.records.size, 0);
  assert.equal(h.sessions.length, 0);
});

test("explicit cancellation still discards queued PC proof and approval while close drains", async () => {
  const h = await harness();
  await h.auth();
  const proof = h.socket.receive(pair.messages.pairProofPc);
  const done = h.socket.receive(pair.messages.pairDone);
  const cancelled = h.gate.last("showPairing").onCancel();
  await Promise.all([proof, done, cancelled]);
  assert.equal(h.gate.screens.at(-1)[0], "showPcList");
  assert.equal(h.gate.screens.some(row => row[1]?.number), false);
  assert.equal(h.records.size, 0);
  assert.equal(h.sessions.length, 0);
});

test("PC denial and window closure show their terminal state without saving", async () => {
  const denied = await harness();
  await denied.compare();
  await denied.socket.receive(pair.messages.pairDenied);
  assert.equal(denied.gate.last("showPairing").state, "denied");
  assert.equal(denied.records.size, 0);
  for (const afterProof of [false, true]) {
    for (const remoteClose of [false, true]) {
      const h = await harness();
      if (afterProof) await h.compare(); else await h.admit();
      if (remoteClose) await h.socket.remoteClose(1000); else await h.socket.receive({ type: "pair-closed" });
      assert.equal(h.gate.last("showPairing").state, "failed");
      assert.equal(h.gate.last("showPairing").message, CLOSED);
      assert.equal(h.records.size, 0);
    }
  }
});

test("1008 before admission gives code guidance, and a missing auth-host reaches the 10 second refusal", async () => {
  const refused = await harness();
  await refused.socket.remoteClose(1008);
  assert.equal(refused.gate.last("showPairing").message,
    "The relay did not accept this code. Check the PC ID and code, or open a new code on the PC.");
  const h = await harness();
  await h.admit();
  assert.equal(h.timers.size, 1);
  const timer = [...h.timers.values()][0];
  assert.equal(timer.ms, 10000);
  timer.callback();
  assert.equal(h.gate.last("showRefusal").title, "PC not reachable");
  assert.equal(h.gate.last("showRefusal").body, "This PC does not support device keys or is not reachable.");
  assert.equal(h.socket.closed, true);
  await h.gate.last("showRefusal").onAction();
  assert.ok(h.gate.last("showPcList"));
});

test("cancellation and locking invalidate pending proof/approval callbacks", async () => {
  for (const action of ["cancel", "lock"]) {
    const h = await harness();
    await h.auth();
    if (action === "cancel") await h.gate.last("showPairing").onCancel();
    else h.app.lock();
    const count = h.gate.screens.length;
    await h.socket.receive(pair.messages.pairProofPc);
    await h.socket.receive(pair.messages.pairDone);
    assert.equal(h.gate.screens.length, count);
    assert.equal(h.records.size, 0);
    assert.equal(h.sessions.length, 0);
    assert.equal(h.timers.size, 0);
  }
});

test("pairing admission refuses a different PC, a normal-session ticket, and invalid opaque mediaEpoch", async () => {
  for (const override of [{ pc: hs.viewer.id }, { ticket: "unexpected" },
    { mediaEpoch: undefined, epoch: hs.sessionEpoch }, { mediaEpoch: 123 }, { mediaEpoch: "" },
    { mediaEpoch: "abc\ndef" }, { mediaEpoch: "é" }, { mediaEpoch: "a".repeat(65) }]) {
    const h = await harness();
    await h.admit(override);
    assert.equal(h.gate.last("showPairing").state, "failed");
    assert.equal(h.socket.sent.some(message => message.type === "auth-hello"), false);
    assert.equal(h.records.size, 0);
  }
});
