// Browser viewer against the native relay (relay-rs, bp-relay). The test starts the relay on loopback
// and drives the real viewer modules over real WebSockets. The PC is a fake written here: it registers
// as the host, publishes the admission list, and answers the handshake and pairing with its own code,
// so every viewer message is checked by a second implementation. Keys, nonces and codes are the public
// vectors in fixtures/device-keys-contract.json. Each test checks its primitives against the contract
// before trusting them.
//
// Known gap: the relay sends mediaEpoch only to the viewer (hello-ok), never to the PC, but the
// transcript needs it. This test hands the viewer's epoch to the fake PC, so a real PC cannot finish
// a handshake from signalling alone.
import { after, before, test } from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createCipheriv, createDecipheriv, createHash, createHmac, hkdfSync, randomBytes } from "node:crypto";
import { once } from "node:events";
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import WebSocket from "ws";
import { createApp } from "../public/dk-app.js";
import { openPairingSocket, registerProof, viewerHello } from "../public/dk-relay.js";
import { createViewerSession } from "../public/dk-session.js";

const here = dirname(fileURLToPath(import.meta.url));
const repo = resolve(here, "..", "..");
const fixture = JSON.parse(readFileSync(join(repo, "fixtures", "device-keys-contract.json"), "utf8"));
const hs = fixture.handshake;
const pairing = fixture.phase2.pairing;
const relayBinary = process.env.BP_RELAY_BIN || join(repo, "relay-rs", "target", "debug", "bp-relay");
const publicDir = join(repo, "worker", "public");
const CASE_TIMEOUT_MS = 15_000;
const TOKEN = randomBytes(24).toString("hex");
const ADMIN_TOKEN = randomBytes(24).toString("hex");
const PKCS8_ED25519 = Buffer.from("302e020100300506032b657004220420", "hex");
const PKCS8_X25519 = Buffer.from("302e020100300506032b656e04220420", "hex");

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

test("normal session: primitives match the contract, then control messages flow both ways", { timeout: CASE_TIMEOUT_MS }, async () => {
  const H = transcriptHash(contractFields());
  assert.equal(H.toString("hex"), hs.transcriptHashHex);
  assert.equal(await signHost(H), hs.pcSignature);
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

  const host = await openHost([VIEWER.id]);
  const viewer = openSessionViewer(VIEWER, PC.publicRaw);
  try {
    const hello = await viewer.raw.take(message => message.type === "hello-ok", "viewer hello-ok");
    assert.equal(hello.pc, PC.id);
    assert.equal(typeof hello.ticket, "string");
    assert.equal(typeof hello.mediaEpoch, "string");

    const authHello = await host.box.take(message => message.type === "auth-hello", "auth-hello");
    assert.equal(authHello.id, VIEWER.id);
    assert.deepEqual(authHello.caps, ["control-aead-v1", "dtls-bind-v1"]);
    const answer = await answerAuth(host, authHello, hello.mediaEpoch);

    const finish = await host.box.take(message => message.type === "auth-finish", "auth-finish");
    assert.equal(await viewerFinishValid(finish, answer), true);
    const pcControl = await controlFor(answer);

    const viewerCaps = pcControl.open(await host.box.take(message => message.type === "ctl" && message.kind === "caps", "viewer caps"));
    assert.deepEqual(viewerCaps.caps, ["control-aead-v1", "dtls-bind-v1"]);
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
  const host = await openHost([VIEWER.id]);
  const stranger = openSessionViewer(STRANGER, null);
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
  const host = await openHost([]);
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
    async load() { return JSON.stringify({ id: VIEWER.id, label: "Viewer", kdf: null, aead: null }); },
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

    const answer = await answerAuth(host, authHello, hello.mediaEpoch);
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
  const host = await openHost([VIEWER.id]);
  // The viewer pins another key, so the real PC key must be refused.
  const viewer = openSessionViewer(VIEWER, STRANGER.publicRaw);
  try {
    const hello = await viewer.raw.take(message => message.type === "hello-ok", "viewer hello-ok");
    const authHello = await host.box.take(message => message.type === "auth-hello", "auth-hello");
    await answerAuth(host, authHello, hello.mediaEpoch);

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

// ---- relay process ----

async function startRelay() {
  if (!existsSync(relayBinary)) {
    throw new Error(`bp-relay not found at ${relayBinary}; build it with cargo build --locked -p bp-relay-server --bin bp-relay`);
  }
  const port = await freePort();
  const origin = `http://127.0.0.1:${port}`;
  const dir = mkdtempSync(join(tmpdir(), "bp-relay-e2e-"));
  const configPath = join(dir, "relay.json");
  writeFileSync(configPath, JSON.stringify({
    public_origin: origin,
    listen: `127.0.0.1:${port}`,
    assets_dir: publicDir,
    state_dir: join(dir, "state"),
    token: TOKEN,
    admin_token: ADMIN_TOKEN,
  }, null, 2));
  let output = "";
  const child = spawn(relayBinary, ["serve", "--config", configPath], { stdio: ["ignore", "pipe", "pipe"] });
  child.stdout.on("data", chunk => { output += chunk; });
  child.stderr.on("data", chunk => { output += chunk; });
  try {
    await waitForHealth(origin, child, () => output);
  } catch (error) {
    await stopChild(child);
    rmSync(dir, { recursive: true, force: true });
    throw error;
  }
  return {
    origin,
    ws: `ws://127.0.0.1:${port}`,
    async stop() {
      await stopChild(child);
      rmSync(dir, { recursive: true, force: true });
    },
  };
}

function freePort() {
  return new Promise((resolvePort, reject) => {
    const server = createServer();
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const { port } = server.address();
      server.close(() => resolvePort(port));
    });
  });
}

async function waitForHealth(origin, child, log) {
  const deadline = Date.now() + 15_000;
  while (Date.now() < deadline) {
    if (child.exitCode !== null) throw new Error(`bp-relay exited early:\n${log()}`);
    try {
      const response = await fetch(`${origin}/healthz`);
      const body = await response.text();
      if (response.ok && body === "ok") return;
    } catch {
      // Not listening yet.
    }
    await delay(100);
  }
  throw new Error(`bp-relay did not answer /healthz:\n${log()}`);
}

async function stopChild(child) {
  if (child.exitCode !== null || child.signalCode !== null) return;
  child.kill("SIGTERM");
  const timer = setTimeout(() => child.kill("SIGKILL"), 3_000);
  await once(child, "exit");
  clearTimeout(timer);
}

// ---- sockets ----

// Messages from one socket in arrival order. take() returns the first match, waiting for it if needed.
// done settles with the close event.
class Mailbox {
  constructor(label) {
    this.label = label;
    this.items = [];
    this.waiters = [];
    this.ended = false;
    this.done = new Promise(resolve => { this.finish = resolve; });
  }

  push(item) {
    const index = this.waiters.findIndex(waiter => waiter.match(item));
    if (index < 0) {
      this.items.push(item);
      return;
    }
    const [waiter] = this.waiters.splice(index, 1);
    clearTimeout(waiter.timer);
    waiter.resolve(item);
  }

  take(match, what, timeoutMs = 10_000) {
    const index = this.items.findIndex(match);
    if (index >= 0) return Promise.resolve(this.items.splice(index, 1)[0]);
    if (this.ended) return Promise.reject(new Error(`${this.label}: closed before ${what}`));
    return new Promise((resolve, reject) => {
      const waiter = { match, resolve, reject, what };
      waiter.timer = setTimeout(() => {
        this.waiters = this.waiters.filter(other => other !== waiter);
        reject(new Error(`${this.label}: timed out waiting for ${what}`));
      }, timeoutMs);
      this.waiters.push(waiter);
    });
  }

  end(info) {
    this.ended = true;
    this.finish(info);
    for (const waiter of this.waiters.splice(0)) {
      clearTimeout(waiter.timer);
      waiter.reject(new Error(`${this.label}: closed (${JSON.stringify(info)}) before ${waiter.what}`));
    }
  }
}

// Feeds a socket's JSON messages and its close into a Mailbox.
function attach(socket, label) {
  const box = new Mailbox(label);
  socket.on("message", data => box.push(JSON.parse(data.toString())));
  socket.on("close", (code, reason) => box.end({ code, reason: reason.toString() }));
  socket.on("error", error => box.end({ error: error.message }));
  return box;
}

function opened(socket) {
  return new Promise((resolveOpen, reject) => {
    socket.once("open", () => resolveOpen());
    socket.once("error", reject);
  });
}

function closeSocket(socket) {
  return new Promise(resolveClose => {
    if (socket.readyState === WebSocket.CLOSED) return resolveClose();
    const timer = setTimeout(() => socket.terminate(), 2_000);
    socket.once("close", () => {
      clearTimeout(timer);
      resolveClose();
    });
    socket.close(1000);
  });
}

// The fake PC: /host-ws registration with its own relay proof, then the admission list, synced by a pong.
async function openHost(admitted) {
  const socket = new WebSocket(`${relay.ws}/host-ws?pc=${PC.id}`);
  const box = attach(socket, "host");
  await opened(socket);
  socket.send(JSON.stringify({ type: "hello", role: "host", token: TOKEN, id: PC.id, public: PC.publicB64 }));
  const challenge = await box.take(message => message.type === "register-challenge", "register-challenge");
  const signature = await relayProof({ role: "host", origin: relay.origin, nonce: Buffer.from(challenge.nonce, "base64url"),
    id: PC.id, publicRaw: PC.publicRaw, privateKey: PC.privateKey });
  socket.send(JSON.stringify({ type: "register-proof", signature }));
  const ok = await box.take(message => message.type === "hello-ok", "host hello-ok");
  assert.equal(ok.pc, PC.id);
  const host = { socket, box };
  await sendThenSync(host, { type: "admit", viewers: admitted, remotePairing: false });
  return host;
}

// The relay handles one connection's messages in order, so the pong shows the message was applied.
async function sendThenSync(host, message) {
  host.socket.send(JSON.stringify(message));
  host.socket.send(JSON.stringify({ type: "ping" }));
  await host.box.take(item => item.type === "pong", "pong");
}

// The browser viewer's signalling side, wired as the page wires it: hello, proof, then one dk-session
// for the admitted socket. Session output goes to `events`; `session()` returns the live session.
function openSessionViewer(identity, pinnedPublic) {
  const socket = new WebSocket(`${relay.ws}/ws?pc=${PC.id}`);
  const raw = attach(socket, "viewer");
  const events = new Mailbox("viewer events");
  let session = null;
  let queue = Promise.resolve();
  socket.on("open", () => socket.send(JSON.stringify(viewerHello({ identity }))));
  socket.on("message", data => {
    const message = JSON.parse(data.toString());
    queue = queue.then(async () => {
      if (!session && message.type === "register-challenge") {
        const proof = await registerProof({ identity, origin: relay.origin, nonce: message.nonce });
        socket.send(JSON.stringify(proof));
      } else if (!session && message.type === "hello-ok") {
        session = createViewerSession({
          identity,
          pcId: PC.id,
          pinnedPublic,
          relayOrigin: relay.origin,
          epoch: message.mediaEpoch,
          transport: "wss",
          name: "e2e viewer",
          send: outgoing => { socket.send(JSON.stringify(outgoing)); return true; },
          onAuthenticated: () => events.push({ type: "authenticated" }),
          onMessage: inner => events.push({ type: "message", message: inner }),
          onFailure: (refusal, reason) => {
            events.push({ type: "failure", reason, refusal });
            socket.close();
          },
        });
        await session.start();
      } else if (session) {
        await session.receive(message);
      }
    }).catch(error => events.push({ type: "error", error }));
  });
  return { socket, raw, events, session: () => session };
}

// ---- fake PC crypto, written from the contract rather than imported from the viewer modules ----

async function loadIdentity(name) {
  const row = fixture.ids.find(entry => entry.name === name);
  const privateKey = await crypto.subtle.importKey("pkcs8",
    Buffer.concat([PKCS8_ED25519, Buffer.from(row.seedHex, "hex")]), { name: "Ed25519" }, false, ["sign"]);
  return { id: row.id, publicB64: row.public, publicRaw: new Uint8Array(Buffer.from(row.public, "base64url")), privateKey };
}

function bytesOf(text) {
  return Buffer.from(text, "base64url");
}

// Each field is a uint32 BE length followed by its bytes.
function lengthPrefixed(parts) {
  return Buffer.concat(parts.flatMap(part => {
    const bytes = typeof part === "string" ? Buffer.from(part, "utf8") : Buffer.from(part);
    const length = Buffer.alloc(4);
    length.writeUInt32BE(bytes.length);
    return [length, bytes];
  }));
}

// The transcript H over the length-prefixed fields, in the order of the contract.
function transcriptHash(fields) {
  return createHash("sha256").update(lengthPrefixed([
    "bubbleplay-auth-v1", fields.origin, fields.epoch, fields.viewerId, fields.viewerPublic, fields.pcId, fields.pcPublic,
    fields.viewerNonce, fields.pcNonce, fields.viewerEphemeral, fields.pcEphemeral,
  ])).digest();
}

function contractFields() {
  return {
    origin: hs.relayOrigin,
    epoch: hs.sessionEpoch,
    viewerId: hs.viewer.id,
    viewerPublic: bytesOf(hs.viewer.public),
    pcId: hs.pc.id,
    pcPublic: bytesOf(hs.pc.public),
    viewerNonce: bytesOf(hs.viewer.nonce),
    pcNonce: bytesOf(hs.pc.nonce),
    viewerEphemeral: bytesOf(hs.viewer.ephemeral),
    pcEphemeral: bytesOf(hs.pc.ephemeral),
  };
}

// The relay's register-proof: Ed25519 signs SHA-256 of the length-prefixed fields, as the 32-byte hash.
async function relayProof({ role, origin, nonce, id, publicRaw, privateKey }) {
  const hash = createHash("sha256").update(lengthPrefixed(["bubbleplay-relay-v1", role, origin, nonce, id, publicRaw])).digest();
  const signature = await crypto.subtle.sign({ name: "Ed25519" }, privateKey, hash);
  return Buffer.from(signature).toString("base64url");
}

// The PC's auth-host signature over "bubbleplay-auth-host-v1" || H.
async function signHost(hash) {
  const signature = await crypto.subtle.sign({ name: "Ed25519" }, PC.privateKey,
    Buffer.concat([Buffer.from("bubbleplay-auth-host-v1"), hash]));
  return Buffer.from(signature).toString("base64url");
}

// The PC's auth-host for one viewer's auth-hello. The epoch comes from the viewer's hello-ok (see the header).
async function answerAuth(host, authHello, epoch) {
  const pcNonce = randomBytes(32);
  const pcEphemeral = await crypto.subtle.generateKey({ name: "X25519" }, true, ["deriveBits"]);
  const pcEphemeralPublic = Buffer.from(await crypto.subtle.exportKey("raw", pcEphemeral.publicKey));
  const viewerPublic = Buffer.from(authHello.public, "base64url");
  const viewerEphemeral = Buffer.from(authHello.ephemeral, "base64url");
  const H = transcriptHash({
    origin: relay.origin,
    epoch,
    viewerId: authHello.id,
    viewerPublic,
    pcId: PC.id,
    pcPublic: PC.publicRaw,
    viewerNonce: Buffer.from(authHello.nonce, "base64url"),
    pcNonce,
    viewerEphemeral,
    pcEphemeral: pcEphemeralPublic,
  });
  const pcSignature = await signHost(H);
  host.socket.send(JSON.stringify({
    type: "auth-host",
    id: PC.id,
    public: PC.publicB64,
    nonce: pcNonce.toString("base64url"),
    ephemeral: pcEphemeralPublic.toString("base64url"),
    signature: pcSignature,
  }));
  return { H, pcSignature, pcPrivate: pcEphemeral.privateKey, viewerPublic, viewerEphemeral };
}

// The viewer's auth-finish: Ed25519 over "bubbleplay-auth-viewer-v1" || H || the PC's signature.
async function viewerFinishValid(finish, answer) {
  const key = await crypto.subtle.importKey("raw", answer.viewerPublic, { name: "Ed25519" }, false, ["verify"]);
  return crypto.subtle.verify({ name: "Ed25519" }, key, Buffer.from(finish.signature, "base64url"),
    Buffer.concat([Buffer.from("bubbleplay-auth-viewer-v1"), answer.H, Buffer.from(answer.pcSignature, "base64url")]));
}

// The PC's control channel: X25519 with the viewer's ephemeral key, HKDF salted with H. The PC sends
// pc-to-viewer and receives viewer-to-pc.
async function controlFor(answer) {
  const peer = await crypto.subtle.importKey("raw", answer.viewerEphemeral, { name: "X25519" }, false, []);
  const shared = Buffer.from(await crypto.subtle.deriveBits({ name: "X25519", public: peer }, answer.pcPrivate, 256));
  const key = info => Buffer.from(hkdfSync("sha256", shared, answer.H, Buffer.from(info, "utf8"), 32));
  return control({ sendKey: key("bubbleplay-ctl-v1 pc-to-viewer"), recvKey: key("bubbleplay-ctl-v1 viewer-to-pc") });
}

// The control keys from the contract's fixed ephemeral pair, to check the derivation and the AEAD framing.
async function contractControlKeys() {
  const pcPrivate = await crypto.subtle.importKey("pkcs8",
    Buffer.concat([PKCS8_X25519, Buffer.from(hs.pc.ephemeralPrivateHex, "hex")]), { name: "X25519" }, false, ["deriveBits"]);
  const viewerEphemeral = await crypto.subtle.importKey("raw", bytesOf(hs.viewer.ephemeral), { name: "X25519" }, false, []);
  const shared = Buffer.from(await crypto.subtle.deriveBits({ name: "X25519", public: viewerEphemeral }, pcPrivate, 256));
  assert.equal(shared.toString("hex"), hs.sharedSecretHex);
  const H = transcriptHash(contractFields());
  const key = info => Buffer.from(hkdfSync("sha256", shared, H, Buffer.from(info, "utf8"), 32));
  return { viewerToPc: key("bubbleplay-ctl-v1 viewer-to-pc"), pcToViewer: key("bubbleplay-ctl-v1 pc-to-viewer") };
}

// Seals and opens JSON control messages. The envelope kind is the message type and is the AAD. The
// nonce is 4 zero bytes and the sequence as uint64 BE; each direction counts from 0.
function control({ sendKey, recvKey }) {
  let sendSeq = 0;
  let recvSeq = 0;
  const nonceFor = seq => {
    const nonce = Buffer.alloc(12);
    nonce.writeBigUInt64BE(BigInt(seq), 4);
    return nonce;
  };
  return {
    seal(message) {
      const cipher = createCipheriv("aes-256-gcm", sendKey, nonceFor(sendSeq));
      cipher.setAAD(Buffer.from(message.type, "utf8"));
      const body = Buffer.concat([cipher.update(Buffer.from(JSON.stringify(message), "utf8")), cipher.final(), cipher.getAuthTag()]);
      const envelope = { type: "ctl", kind: message.type, seq: sendSeq, data: body.toString("base64url") };
      sendSeq += 1;
      return envelope;
    },
    open(envelope) {
      assert.equal(envelope.seq, recvSeq, "control sequence");
      const raw = Buffer.from(envelope.data, "base64url");
      const decipher = createDecipheriv("aes-256-gcm", recvKey, nonceFor(recvSeq));
      decipher.setAAD(Buffer.from(envelope.kind, "utf8"));
      decipher.setAuthTag(raw.subarray(raw.length - 16));
      const plain = Buffer.concat([decipher.update(raw.subarray(0, raw.length - 16)), decipher.final()]);
      recvSeq += 1;
      const message = JSON.parse(plain.toString("utf8"));
      assert.equal(message.type, envelope.kind);
      return message;
    },
  };
}

// The relay's admit hash: SHA-256 of "bubbleplay-pair-admit-v1", the PC ID and the code, concatenated with no prefixes.
function pairAdmitFor(pcId, code) {
  return createHash("sha256")
    .update(Buffer.from("bubbleplay-pair-admit-v1", "utf8"))
    .update(Buffer.from(pcId, "utf8"))
    .update(Buffer.from(code, "utf8"))
    .digest("base64url");
}

// The pair key: HKDF-SHA256 of the code, empty salt, info "bubbleplay-pair-v1".
function pairKeyFor(code) {
  return Buffer.from(hkdfSync("sha256", Buffer.from(code, "utf8"), Buffer.alloc(0), Buffer.from("bubbleplay-pair-v1", "utf8"), 32));
}

// HMAC-SHA256(key, role || H).
function pairMacFor(key, role, hash) {
  return createHmac("sha256", key).update(role, "utf8").update(hash).digest();
}

// The six-digit comparison: the first 4 bytes of SHA-256("bubbleplay-pair-sas-v1" || H), mod 10^6.
function comparisonNumberFor(hash) {
  const digest = createHash("sha256").update("bubbleplay-pair-sas-v1", "utf8").update(hash).digest();
  return String(digest.readUInt32BE(0) % 1_000_000).padStart(6, "0");
}

function delay(ms) {
  return new Promise(resolveDelay => setTimeout(resolveDelay, ms));
}
