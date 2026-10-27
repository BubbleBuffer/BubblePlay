import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import {
  b64urlDecode, b64urlEncode, normalizePairingCode, openPairingSocket, pairAdmitHash, pcListUrl,
  registerProof, relayOrigin, relayProofEncoding, relayProofHash, viewerHello,
} from "../public/dk-relay.js";

const contract = JSON.parse(readFileSync(new URL("../../fixtures/device-keys-contract.json", import.meta.url)));
const VIEWER_VECTOR = contract.relayProof.vectors.find(vector => vector.name === "viewer");
const PKCS8_ED25519_PREFIX = Buffer.from("302e020100300506032b657004220420", "hex");

// The fixture identity whose public key the viewer vector uses (test-only seed).
async function fixtureIdentity(publicText) {
  const entry = contract.ids.find(item => item.public === publicText);
  assert.ok(entry, "fixture identity for the vector");
  const privateKey = await crypto.subtle.importKey("pkcs8",
    Buffer.concat([PKCS8_ED25519_PREFIX, Buffer.from(entry.seedHex, "hex")]), { name: "Ed25519" }, false, ["sign"]);
  return { id: entry.id, publicRaw: b64urlDecode(entry.public, 32), privateKey };
}

test("relay proof for the viewer fixture vector has identical encoding, H and signature", async () => {
  assert.equal(VIEWER_VECTOR.role, "viewer");
  const identity = await fixtureIdentity(VIEWER_VECTOR.public);
  assert.equal(identity.id, VIEWER_VECTOR.id);
  const fields = { role: "viewer", origin: VIEWER_VECTOR.origin, nonce: b64urlDecode(VIEWER_VECTOR.nonce, 32), id: identity.id, publicRaw: identity.publicRaw };
  assert.equal(Buffer.from(relayProofEncoding(fields)).toString("hex"), VIEWER_VECTOR.encodingHex);
  assert.equal(Buffer.from(await relayProofHash(fields)).toString("hex"), VIEWER_VECTOR.sha256Hex);

  const proof = await registerProof({ identity, origin: VIEWER_VECTOR.origin, nonce: VIEWER_VECTOR.nonce });
  assert.deepEqual(Object.keys(proof), ["type", "signature"]);
  assert.equal(proof.type, "register-proof");
  // Ed25519 is deterministic: the same key and H give the fixture's signature.
  assert.equal(proof.signature, VIEWER_VECTOR.signature);
  const verifier = await crypto.subtle.importKey("raw", identity.publicRaw, { name: "Ed25519" }, false, ["verify"]);
  assert.ok(await crypto.subtle.verify({ name: "Ed25519" }, verifier, b64urlDecode(proof.signature, 64),
    Buffer.from(VIEWER_VECTOR.sha256Hex, "hex")));
  // A malformed nonce gets no proof.
  for (const nonce of [undefined, "", "AAAA", VIEWER_VECTOR.nonce + "A"])
    assert.equal(await registerProof({ identity, origin: VIEWER_VECTOR.origin, nonce }), null);
});

test("relay origin is canonicalised like relay-rs", () => {
  for (const [input, expected] of [
    ["https://Play.Example.test/pair?x=1#y", "https://play.example.test"],
    ["https://play.example.test:443/", "https://play.example.test"],
    ["https://play.example.test:8443/", "https://play.example.test:8443"],
    ["wss://play.example.test/ws", "https://play.example.test"],
    ["http://localhost:80/", "http://localhost"],
    ["ws://127.0.0.1:8787/", "http://127.0.0.1:8787"],
    ["https://user:pw@play.example.test/", null],
    ["ftp://play.example.test/", null],
    ["not a url", null],
  ]) assert.equal(relayOrigin(input), expected, input);
});

test("viewer hello has exactly the protocol keys", async () => {
  const identity = await fixtureIdentity(VIEWER_VECTOR.public);
  assert.deepEqual(viewerHello({ identity }), { type: "hello", role: "viewer", id: VIEWER_VECTOR.id, public: VIEWER_VECTOR.public });
  assert.deepEqual(Object.keys(viewerHello({ identity, turn: true, pair: "p" })), ["type", "role", "id", "public", "turn", "pair"]);
  assert.equal(viewerHello({ identity, turn: false }).turn, undefined);
});

test("pairing codes normalise like IDs and the admit hash covers PC ID and code", async () => {
  assert.equal(normalizePairingCode("7k3qd-m9x2p"), "7K3QDM9X2P");
  assert.equal(normalizePairingCode(" oiL0O 12345 "), "0110012345");
  for (const bad of ["", "7K3QD-M9X2", "7K3QD-M9X2PP", "7K3QD-M9X2U", "7K3QD_M9X2P", null]) assert.equal(normalizePairingCode(bad), null);
  const pc = contract.ids.find(item => item.name === "pc").id;
  const expected = b64urlEncode(new Uint8Array(await crypto.subtle.digest("SHA-256",
    new TextEncoder().encode(`bubbleplay-pair-admit-v1${pc}7K3QDM9X2P`))));
  assert.equal(await pairAdmitHash(pc, "7K3QDM9X2P"), expected);
  assert.equal(b64urlDecode(expected, 32).length, 32);
  assert.notEqual(await pairAdmitHash(pc, "7K3QDM9X2Q"), expected);
});

test("PC list URL drops pc and the fragment and keeps other parameters", () => {
  assert.equal(pcListUrl("https://relay.test/pair?pc=AAAA&transport=wss#pc=x&code=y"), "https://relay.test/?transport=wss");
});

function fakeSocketClass() {
  const sockets = [];
  return { sockets, Socket: class {
    constructor(url) { this.url = url; this.sent = []; this.closed = false; sockets.push(this); }
    send(text) { this.sent.push(JSON.parse(text)); }
    close() { this.closed = true; }
  } };
}

test("pairing socket: signed hello admits pairing and routes subsequent messages in order", async () => {
  const identity = await fixtureIdentity(VIEWER_VECTOR.public);
  const { sockets, Socket } = fakeSocketClass();
  const events = [];
  openPairingSocket({ identity, pc: "77VH-7M56-8GJK-Y12J", pair: "PAIR", href: "https://example.test/pair",
    WebSocketImpl: Socket, onPairingAdmitted: ({ message }) => events.push(["admitted", message.pairing]),
    onMessage: message => events.push(["message", message.type]),
    onRefused: () => events.push(["refused"]), onClosed: () => events.push(["closed"]) });
  const socket = sockets[0];
  assert.equal(socket.url, "wss://example.test/ws?pc=77VH-7M56-8GJK-Y12J");
  socket.onopen();
  assert.deepEqual(socket.sent[0], { type: "hello", role: "viewer", id: identity.id, public: VIEWER_VECTOR.public, pair: "PAIR" });
  await socket.onmessage({ data: JSON.stringify({ type: "register-challenge", nonce: VIEWER_VECTOR.nonce }) });
  assert.deepEqual(socket.sent[1], { type: "register-proof", signature: VIEWER_VECTOR.signature });
  await socket.onmessage({ data: JSON.stringify({ type: "hello-ok", pc: "77VH-7M56-8GJK-Y12J", pairing: true }) });
  assert.deepEqual(events, [["admitted", true]]);
  assert.equal(socket.sent.length, 2);
  await socket.onmessage({ data: JSON.stringify({ type: "pair-proof", mac: "m" }) });
  await socket.onmessage({ data: JSON.stringify({ type: "pair-done" }) });
  assert.deepEqual(events, [["admitted", true], ["message", "pair-proof"], ["message", "pair-done"]]);
});

test("pairing socket serializes asynchronous admission and closes on callback errors", async () => {
  const identity = await fixtureIdentity(VIEWER_VECTOR.public);
  const { sockets, Socket } = fakeSocketClass();
  const events = [];
  let release;
  const pending = new Promise(resolve => { release = resolve; });
  openPairingSocket({ identity, pc: "77VH-7M56-8GJK-Y12J", pair: "PAIR", href: "https://example.test/pair",
    WebSocketImpl: Socket,
    onPairingAdmitted: async () => { events.push("begin"); await pending; events.push("ready"); },
    onMessage: message => { events.push(message.type); throw new Error("bad proof"); },
    onError: error => events.push(error.message) });
  const socket = sockets[0];
  const admission = socket.onmessage({ data: JSON.stringify({ type: "hello-ok", pairing: true }) });
  const proof = socket.onmessage({ data: JSON.stringify({ type: "pair-proof", mac: "m" }) });
  await Promise.resolve();
  assert.deepEqual(events, ["begin"]);
  release();
  await admission;
  await proof;
  assert.deepEqual(events, ["begin", "ready", "pair-proof", "bad proof"]);
  assert.equal(socket.closed, true);
});

test("pairing socket: 1008 before hello-ok is a refusal; hello-ok without pairing closes", async () => {
  const identity = await fixtureIdentity(VIEWER_VECTOR.public);
  const { sockets, Socket } = fakeSocketClass();
  const events = [];
  const open = () => openPairingSocket({ identity, pc: "77VH-7M56-8GJK-Y12J", pair: "PAIR", href: "https://example.test/pair",
    WebSocketImpl: Socket, onPairingAdmitted: () => events.push("admitted"),
    onRefused: () => events.push("refused"), onClosed: () => events.push("closed") });
  open();
  sockets[0].onopen();
  sockets[0].onclose({ code: 1008 });
  open();
  await sockets[1].onmessage({ data: JSON.stringify({ type: "hello-ok", ticket: "t" }) });
  assert.ok(sockets[1].closed);
  assert.deepEqual(events, ["refused"]);
});
