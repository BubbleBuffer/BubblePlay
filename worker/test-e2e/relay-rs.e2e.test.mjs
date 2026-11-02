// Browser viewer against the native relay (relay-rs, bp-relay): a normal session, a stranger, pairing from a /pair
// link, and a PC key that differs from the pinned one. The harness starts the relay and the fake PC, and each test
// checks its primitives against the contract before trusting them.
//
// The fake PC takes the handshake epoch from peer-joined on its own /host-ws, the way a real PC does. The relay queues
// that message before the viewer's hello-ok, so the epoch comes from the relay, not from the viewer.
import { after, before, test } from "node:test";
import assert from "node:assert/strict";
import WebSocket from "ws";
import { createApp } from "../public/dk-app.js";
import { openPairingSocket } from "../public/dk-relay.js";
import {
  CASE_TIMEOUT_MS, Mailbox, answerAuth, assertEpochBeforeAuth, bytesOf, closeSocket, comparisonNumberFor, contractControlKeys,
  contractFields, control, controlFor, delay, fixture, hs, loadIdentity, openHost, openSessionViewer, pairAdmitFor, pairKeyFor,
  pairMacFor, pairing, relayProof, sendThenSync, signHost, startRelay, transcriptHash, viewerFinishValid,
} from "./harness.mjs";

let relay = null;
let PC = null;
let VIEWER = null;
let STRANGER = null;

before(async () => {
  PC = await loadIdentity("pc");
  VIEWER = await loadIdentity("viewer");
  STRANGER = await loadIdentity("crockford-aliases");
  relay = await startRelay();
});

after(async () => {
  await relay?.stop();
  relay = null;
});

test("normal session: primitives match the contract, then control messages flow both ways", { timeout: CASE_TIMEOUT_MS }, async t => {
  const H = transcriptHash(contractFields());
  assert.equal(H.toString("hex"), hs.transcriptHashHex);
  assert.equal(await signHost(PC, H), hs.pcSignature);
  const relayVector = fixture.relayProof.vectors.find(vector => vector.name === "host");
  assert.equal(await relayProof({ role: "host", origin: relayVector.origin, nonce: bytesOf(relayVector.nonce), id: PC.id,
    publicRaw: PC.publicRaw, privateKey: PC.privateKey }), relayVector.signature);
  const keys = await contractControlKeys();
  assert.equal(keys.viewerToPc.toString("hex"), hs.controlKeys.viewerToPcHex);
  assert.equal(keys.pcToViewer.toString("hex"), hs.controlKeys.pcToViewerHex);
  const contractMessage = (direction, sequence) => hs.controlMessages.find(item => item.direction === direction && item.sequence === sequence);
  const viewerInput = contractMessage("viewer-to-pc", 0);
  assert.equal(control({ sendKey: keys.viewerToPc, recvKey: keys.pcToViewer }).seal(JSON.parse(viewerInput.plaintextUtf8)).data, viewerInput.ciphertext);
  const pcClipboard = contractMessage("pc-to-viewer", 0);
  assert.equal(control({ sendKey: keys.pcToViewer, recvKey: keys.viewerToPc }).seal(JSON.parse(pcClipboard.plaintextUtf8)).data, pcClipboard.ciphertext);

  const arrivals = [];
  const host = await openHost(relay, PC, [VIEWER.id], arrivals);
  const viewer = openSessionViewer(relay, VIEWER, PC.publicRaw, PC, arrivals);
  try {
    const hello = await viewer.raw.take(message => message.type === "hello-ok", "viewer hello-ok");
    assert.equal(hello.pc, PC.id);
    assert.equal(typeof hello.ticket, "string");
    assert.equal(typeof hello.mediaEpoch, "string");

    // The epoch the PC signs comes from its own peer-joined, and that is the epoch the viewer holds too.
    const joined = await host.box.take(message => message.type === "peer-joined", "peer-joined");
    assert.equal(joined.mediaEpoch, hello.mediaEpoch);
    assert.equal(joined.pairing, false);

    const authHello = await host.box.take(message => message.type === "auth-hello", "auth-hello");
    assert.equal(authHello.id, VIEWER.id);
    assert.deepEqual(authHello.caps, ["control-aead-v1", "dtls-bind-v1"]);
    assertEpochBeforeAuth(t, arrivals);
    const answer = await answerAuth(host, authHello, joined.mediaEpoch);

    const finish = await host.box.take(message => message.type === "auth-finish", "auth-finish");
    assert.equal(await viewerFinishValid(finish, answer), true);
    const pcControl = await controlFor(answer);

    const viewerCaps = pcControl.open(await host.box.take(message => message.type === "ctl" && message.kind === "caps", "viewer caps"));
    assert.deepEqual(viewerCaps.caps, ["control-aead-v1", "dtls-bind-v1", "client-logs-v1"]);
    host.socket.send(JSON.stringify(pcControl.seal({ type: "caps", caps: ["control-aead-v1", "dtls-bind-v1"] })));
    await viewer.events.take(event => event.type === "authenticated", "viewer authenticated");
    assert.equal(viewer.session().controlReady, true);

    viewer.session().sendControl({ type: "clipboard", text: "from-viewer" });
    const fromViewer = pcControl.open(await host.box.take(message => message.type === "ctl" && message.kind === "clipboard", "viewer clipboard"));
    assert.equal(fromViewer.text, "from-viewer");

    host.socket.send(JSON.stringify(pcControl.seal({ type: "clipboard", text: "from-pc" })));
    const fromPc = await viewer.events.take(event => event.type === "message" && event.message.type === "clipboard", "PC clipboard");
    assert.equal(fromPc.message.text, "from-pc");
  } finally {
    await closeSocket(viewer.socket);
    await closeSocket(host.socket);
  }
});

test("a viewer that is not on the admission list is closed 1008 before hello-ok", { timeout: CASE_TIMEOUT_MS }, async () => {
  const host = await openHost(relay, PC, [VIEWER.id]);
  const stranger = openSessionViewer(relay, STRANGER, null, PC);
  try {
    const close = await stranger.raw.done;
    assert.equal(close.code, 1008);
    assert.equal(stranger.raw.items.some(message => message.type === "hello-ok"), false);
    assert.equal(stranger.events.items.length, 0);
  } finally {
    await closeSocket(stranger.socket);
    await closeSocket(host.socket);
  }
});

test("pairing from a /pair link: the same number on both sides, then pair-done", { timeout: CASE_TIMEOUT_MS }, async () => {
  const fixtureHash = Buffer.from(hs.transcriptHashHex, "hex");
  const key = pairKeyFor(pairing.code);
  assert.equal(pairAdmitFor(PC.id, pairing.code), pairing.admit);
  assert.equal(key.toString("hex"), pairing.pairKeyHex);
  assert.equal(pairMacFor(key, "viewer", fixtureHash).toString("base64url"), pairing.viewerMac);
  assert.equal(pairMacFor(key, "pc", fixtureHash).toString("base64url"), pairing.pcMac);
  assert.equal(comparisonNumberFor(fixtureHash), pairing.sasNumber);

  // The admit list is empty, so the viewer can only get in through the pairing window.
  const host = await openHost(relay, PC, []);
  await sendThenSync(host, { type: "pair-open", admit: pairing.admit });

  const admission = new Mailbox("pairing admission");
  const screens = new Mailbox("pairing screens");
  const sockets = [];
  const gate = {
    showKeyScreen() {},
    showPcList() {},
    showBusy() {},
    hide() {},
    showRefusal(options) { screens.push({ state: "refusal", ...options }); },
    showPairing(options) { screens.push(options); },
  };
  const location = { href: `${relay.origin}/pair#pc=${PC.id}&code=${pairing.code}`, reload() {} };
  const history = { replaceState(_state, _title, url) { location.href = new URL(url, location.href).href; } };
  const store = {
    async load() { return JSON.stringify({ format: "bubblebuffer-device-key", id: VIEWER.id, label: "Viewer", kdf: null, aead: null }); },
    async listPcs() { return []; },
    async getPc() { return null; },
    async putPc() { throw new Error("pair-done must not save before the user confirms"); },
  };
  const app = createApp({
    gate,
    store,
    location,
    history,
    unlock: async () => ({ id: VIEWER.id, publicKey: VIEWER.publicRaw, privateKey: VIEWER.privateKey }),
    openPairing: options => {
      const socket = openPairingSocket({
        ...options,
        WebSocketImpl: WebSocket,
        onPairingAdmitted: args => {
          admission.push(args.message);
          return options.onPairingAdmitted(args);
        },
      });
      sockets.push(socket);
      return socket;
    },
    now: () => 1,
  });
  try {
    await app.start();
    const authHello = await host.box.take(message => message.type === "auth-hello", "pairing auth-hello");
    assert.deepEqual(authHello.caps, ["control-aead-v1", "dtls-bind-v1", "pair-v1"]);
    const hello = await admission.take(() => true, "pairing hello-ok");
    assert.equal(hello.pairing, true);
    assert.equal(hello.pc, PC.id);
    assert.equal(hello.ticket, undefined);

    // The PC learns the pairing epoch from peer-joined, the same way as a normal session.
    const joined = await host.box.take(message => message.type === "peer-joined", "pairing peer-joined");
    assert.equal(joined.pairing, true);
    assert.equal(joined.mediaEpoch, hello.mediaEpoch);

    const answer = await answerAuth(host, authHello, joined.mediaEpoch);
    const finish = await host.box.take(message => message.type === "auth-finish", "auth-finish");
    assert.equal(await viewerFinishValid(finish, answer), true);

    const viewerProof = await host.box.take(message => message.type === "pair-proof", "viewer pair-proof");
    assert.equal(viewerProof.mac, pairMacFor(key, "viewer", answer.H).toString("base64url"));
    host.socket.send(JSON.stringify({ type: "pair-proof", mac: pairMacFor(key, "pc", answer.H).toString("base64url") }));

    const compare = await screens.take(screen => screen.state === "compare", "compare screen");
    assert.equal(compare.number, comparisonNumberFor(answer.H));

    host.socket.send(JSON.stringify({ type: "pair-done" }));
    const done = await screens.take(screen => screen.state === "done", "done screen");
    assert.equal(typeof done.onSave, "function");
  } finally {
    for (const socket of sockets) await closeSocket(socket);
    await closeSocket(host.socket);
  }
});

test("a PC key that differs from the pinned key is refused before auth-finish is sent", { timeout: CASE_TIMEOUT_MS }, async () => {
  const host = await openHost(relay, PC, [VIEWER.id]);
  // The viewer pins another key, so the real PC key must be refused.
  const viewer = openSessionViewer(relay, VIEWER, STRANGER.publicRaw, PC);
  try {
    const hello = await viewer.raw.take(message => message.type === "hello-ok", "viewer hello-ok");
    const joined = await host.box.take(message => message.type === "peer-joined", "peer-joined");
    assert.equal(joined.mediaEpoch, hello.mediaEpoch);
    const authHello = await host.box.take(message => message.type === "auth-hello", "auth-hello");
    await answerAuth(host, authHello, joined.mediaEpoch);

    const failure = await viewer.events.take(event => event.type === "failure", "viewer refusal");
    assert.equal(failure.reason, "pc-key-changed");
    assert.equal(failure.refusal.title, "PC key changed");

    // A late frame from the viewer would reach the PC after the refusal, so give it time to arrive.
    await delay(300);
    assert.equal(host.box.items.some(message => message.type === "auth-finish"), false);
    assert.equal(host.box.items.some(message => message.type === "ctl"), false);
  } finally {
    await closeSocket(viewer.socket);
    await closeSocket(host.socket);
  }
});
