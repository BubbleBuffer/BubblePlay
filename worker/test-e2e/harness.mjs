// Shared helpers for the browser viewer's end-to-end tests against the native relay (relay-rs, bp-relay).
// Each test file starts its own relay on loopback and drives the real viewer modules over real WebSockets.
// The PC is a fake written here from the contracts: it registers as the host, publishes the admission list,
// and answers the handshake, pairing and file transfer with its own code, so every viewer message is checked
// by a second implementation. Keys, nonces and codes are the public vectors in fixtures/device-keys-contract.json.
//
// The relay sends mediaEpoch to the viewer (hello-ok) and to the PC (peer-joined on /host-ws). The fake PC takes
// the epoch from peer-joined, so a PC that only has the signalling can finish a handshake.
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
import { registerProof, viewerHello } from "../public/dk-relay.js";
import { createViewerSession } from "../public/dk-session.js";

const here = dirname(fileURLToPath(import.meta.url));
const repo = resolve(here, "..", "..");
export const fixture = JSON.parse(readFileSync(join(repo, "fixtures", "device-keys-contract.json"), "utf8"));
export const hs = fixture.handshake;
export const pairing = fixture.phase2.pairing;
const relayBinaryName = process.platform === "win32" ? "bp-relay.exe" : "bp-relay";
const relayBinary = process.env.BP_RELAY_BIN || join(repo, "relay-rs", "target", "debug", relayBinaryName);
const publicDir = join(repo, "worker", "public");
export const CASE_TIMEOUT_MS = 15_000;
export const TOKEN = randomBytes(24).toString("hex");
const ADMIN_TOKEN = randomBytes(24).toString("hex");
const PKCS8_ED25519 = Buffer.from("302e020100300506032b657004220420", "hex");
const PKCS8_X25519 = Buffer.from("302e020100300506032b656e04220420", "hex");
const CROCKFORD = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";

// ---- relay process ----

export async function startRelay() {
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

// ---- identities ----

// The fixture identities (pc, viewer, crockford-aliases). Each ID must derive from its key, as the relay checks.
export async function loadIdentity(name) {
  const row = fixture.ids.find(entry => entry.name === name);
  const privateKey = await crypto.subtle.importKey("pkcs8",
    Buffer.concat([PKCS8_ED25519, Buffer.from(row.seedHex, "hex")]), { name: "Ed25519" }, false, ["sign"]);
  const publicRaw = new Uint8Array(Buffer.from(row.public, "base64url"));
  assert.equal(deviceIdFor(publicRaw), row.id, `fixture ID for ${name} derives from its key`);
  return { id: row.id, publicB64: row.public, publicRaw, privateKey };
}

// The device ID: the first 80 bits of SHA-256 over the raw public key, in Crockford base32, in four groups.
export function deviceIdFor(publicRaw) {
  const digest = createHash("sha256").update(publicRaw).digest();
  let bits = 0n;
  for (const byte of digest.subarray(0, 10)) bits = (bits << 8n) | BigInt(byte);
  let text = "";
  for (let index = 0n; index < 16n; index += 1n) text += CROCKFORD[Number((bits >> (75n - 5n * index)) & 31n)];
  return text.match(/.{4}/g).join("-");
}

// ---- sockets ----

// Messages from one socket in arrival order. take() returns the first match, waiting for it if needed.
// done settles with the close event.
export class Mailbox {
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

// Feeds a socket's messages and its close into a Mailbox. Text is parsed as JSON; a binary frame arrives as
// { binary }. With `arrivals`, every message is also logged as { label, type } in the order it reached this process.
export function attach(socket, label, arrivals = null) {
  const box = new Mailbox(label);
  socket.on("message", (data, isBinary) => {
    const message = isBinary ? { binary: Buffer.from(data) } : JSON.parse(data.toString());
    arrivals?.push({ label, type: message.type ?? "binary" });
    box.push(message);
  });
  socket.on("close", (code, reason) => box.end({ code, reason: reason.toString() }));
  socket.on("error", error => box.end({ error: error.message }));
  return box;
}

export function opened(socket) {
  return new Promise((resolveOpen, reject) => {
    socket.once("open", () => resolveOpen());
    socket.once("error", reject);
  });
}

export function closeSocket(socket) {
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

export function delay(ms) {
  return new Promise(resolveDelay => setTimeout(resolveDelay, ms));
}

// The HTTP status an upgrade is refused with. A refused data socket gets a 404 before any upgrade.
export function upgradeStatus(url) {
  return new Promise((resolveStatus, reject) => {
    const socket = new WebSocket(url);
    socket.on("error", reject);
    socket.once("open", () => {
      socket.close();
      reject(new Error("upgrade was accepted"));
    });
    socket.once("unexpected-response", (_request, response) => {
      response.resume();
      socket.terminate();
      resolveStatus(response.statusCode);
    });
  });
}

// ---- the fake PC's signalling ----

// /host-ws: registers with its own relay proof, admits the viewers, then syncs on a pong. The hello-ok ticket is
// the PC's ticket for its data sockets, so `host.ticket` is what the fake PC sends on /host-file-ws.
export async function openHost(relay, pc, admitted, arrivals = null) {
  const socket = new WebSocket(`${relay.ws}/host-ws?pc=${pc.id}`);
  const box = attach(socket, "pc", arrivals);
  await opened(socket);
  socket.send(JSON.stringify({ type: "hello", role: "host", token: TOKEN, id: pc.id, public: pc.publicB64 }));
  const challenge = await box.take(message => message.type === "register-challenge", "register-challenge");
  const signature = await relayProof({ role: "host", origin: relay.origin, nonce: Buffer.from(challenge.nonce, "base64url"),
    id: pc.id, publicRaw: pc.publicRaw, privateKey: pc.privateKey });
  socket.send(JSON.stringify({ type: "register-proof", signature }));
  const ok = await box.take(message => message.type === "hello-ok", "host hello-ok");
  assert.equal(ok.pc, pc.id);
  const host = { socket, box, pc, origin: relay.origin, ticket: ok.ticket };
  await sendThenSync(host, { type: "admit", viewers: admitted, remotePairing: false });
  return host;
}

// The relay handles one connection's messages in order, so the pong shows the message was applied.
export async function sendThenSync(host, message) {
  host.socket.send(JSON.stringify(message));
  host.socket.send(JSON.stringify({ type: "ping" }));
  await host.box.take(item => item.type === "pong", "pong");
}

// The fake PC's auth-host for one viewer's auth-hello. The epoch is the one the PC got in peer-joined.
export async function answerAuth(host, authHello, epoch) {
  const pcNonce = randomBytes(32);
  const pcEphemeral = await crypto.subtle.generateKey({ name: "X25519" }, true, ["deriveBits"]);
  const pcEphemeralPublic = Buffer.from(await crypto.subtle.exportKey("raw", pcEphemeral.publicKey));
  const viewerPublic = Buffer.from(authHello.public, "base64url");
  const viewerEphemeral = Buffer.from(authHello.ephemeral, "base64url");
  const H = transcriptHash({
    origin: host.origin,
    epoch,
    viewerId: authHello.id,
    viewerPublic,
    pcId: host.pc.id,
    pcPublic: host.pc.publicRaw,
    viewerNonce: Buffer.from(authHello.nonce, "base64url"),
    pcNonce,
    viewerEphemeral,
    pcEphemeral: pcEphemeralPublic,
  });
  const pcSignature = await signHost(host.pc, H);
  host.socket.send(JSON.stringify({
    type: "auth-host",
    id: host.pc.id,
    public: host.pc.publicB64,
    nonce: pcNonce.toString("base64url"),
    ephemeral: pcEphemeralPublic.toString("base64url"),
    signature: pcSignature,
  }));
  return { H, pcSignature, pcPrivate: pcEphemeral.privateKey, viewerPublic, viewerEphemeral };
}

// The PC must know the epoch before it can answer the viewer's auth-hello. The relay queues peer-joined on the PC's
// socket before the viewer's hello-ok, and the viewer sends auth-hello only after hello-ok, so on the PC's socket
// peer-joined comes first. The relay gives no order across two sockets, so that order is logged, not asserted.
export function assertEpochBeforeAuth(t, arrivals) {
  const pcTypes = arrivals.filter(entry => entry.label === "pc").map(entry => entry.type);
  assert.ok(pcTypes.includes("peer-joined"), "the PC got peer-joined");
  assert.ok(pcTypes.indexOf("peer-joined") < pcTypes.indexOf("auth-hello"), `PC socket order: ${pcTypes.join(" > ")}`);
  t.diagnostic(`arrival order: ${arrivals.map(entry => `${entry.label}:${entry.type}`).join(" > ")}`);
}

// ---- viewers ----

// The browser viewer's signalling side, wired as the page wires it: hello, proof, then one dk-session for the
// admitted socket. Session output goes to `events`; `session()` returns the live session.
export function openSessionViewer(relay, identity, pinnedPublic, pc, arrivals = null) {
  const socket = new WebSocket(`${relay.ws}/ws?pc=${pc.id}`);
  const raw = attach(socket, "viewer", arrivals);
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
          pcId: pc.id,
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

// A viewer's signalling socket up to its register-proof. The caller reads hello-ok or the close. `pair` is the
// pairing hash for /ws in pairing mode.
export async function connectViewer(relay, identity, pcId, { pair = null } = {}) {
  const socket = new WebSocket(`${relay.ws}/ws?pc=${pcId}`);
  const box = attach(socket, "viewer");
  await opened(socket);
  socket.send(JSON.stringify(viewerHello({ identity, pair })));
  const challenge = await box.take(message => message.type === "register-challenge", "viewer register-challenge");
  socket.send(JSON.stringify(await registerProof({ identity, origin: relay.origin, nonce: challenge.nonce })));
  return { socket, box };
}

// A viewer admitted on its signalling socket: its hello-ok carries the ticket and the epoch.
export async function admitViewer(relay, identity, pcId) {
  const viewer = await connectViewer(relay, identity, pcId);
  viewer.hello = await viewer.box.take(message => message.type === "hello-ok", "viewer hello-ok");
  return viewer;
}

// A data socket (/media-ws, /file-ws, /host-media-ws or /host-file-ws) with its hello sent. A null epoch leaves
// the query without one.
export async function openDataSocket(relay, path, pcId, epoch, hello) {
  const query = new URLSearchParams({ pc: pcId });
  if (epoch !== null) query.set("epoch", epoch);
  const socket = new WebSocket(`${relay.ws}${path}?${query}`);
  const box = attach(socket, path.slice(1));
  await opened(socket);
  socket.send(JSON.stringify(hello));
  return { socket, box };
}

// ---- fake PC crypto, written from the contract rather than imported from the viewer modules ----

export function bytesOf(text) {
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
export function transcriptHash(fields) {
  return createHash("sha256").update(lengthPrefixed([
    "bubbleplay-auth-v1", fields.origin, fields.epoch, fields.viewerId, fields.viewerPublic, fields.pcId, fields.pcPublic,
    fields.viewerNonce, fields.pcNonce, fields.viewerEphemeral, fields.pcEphemeral,
  ])).digest();
}

export function contractFields() {
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
export async function relayProof({ role, origin, nonce, id, publicRaw, privateKey }) {
  const hash = createHash("sha256").update(lengthPrefixed(["bubbleplay-relay-v1", role, origin, nonce, id, publicRaw])).digest();
  const signature = await crypto.subtle.sign({ name: "Ed25519" }, privateKey, hash);
  return Buffer.from(signature).toString("base64url");
}

// The PC's auth-host signature over "bubbleplay-auth-host-v1" || H.
export async function signHost(pc, hash) {
  const signature = await crypto.subtle.sign({ name: "Ed25519" }, pc.privateKey,
    Buffer.concat([Buffer.from("bubbleplay-auth-host-v1"), hash]));
  return Buffer.from(signature).toString("base64url");
}

// The viewer's auth-finish: Ed25519 over "bubbleplay-auth-viewer-v1" || H || the PC's signature.
export async function viewerFinishValid(finish, answer) {
  const key = await crypto.subtle.importKey("raw", answer.viewerPublic, { name: "Ed25519" }, false, ["verify"]);
  return crypto.subtle.verify({ name: "Ed25519" }, key, Buffer.from(finish.signature, "base64url"),
    Buffer.concat([Buffer.from("bubbleplay-auth-viewer-v1"), answer.H, Buffer.from(answer.pcSignature, "base64url")]));
}

// The PC's control channel: X25519 with the viewer's ephemeral key, HKDF salted with H. The PC sends
// pc-to-viewer and receives viewer-to-pc.
export async function controlFor(answer) {
  const peer = await crypto.subtle.importKey("raw", answer.viewerEphemeral, { name: "X25519" }, false, []);
  const shared = Buffer.from(await crypto.subtle.deriveBits({ name: "X25519", public: peer }, answer.pcPrivate, 256));
  const key = info => Buffer.from(hkdfSync("sha256", shared, answer.H, Buffer.from(info, "utf8"), 32));
  return control({ sendKey: key("bubbleplay-ctl-v1 pc-to-viewer"), recvKey: key("bubbleplay-ctl-v1 viewer-to-pc") });
}

// The control keys from the contract's fixed ephemeral pair, to check the derivation and the AEAD framing.
export async function contractControlKeys() {
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
export function control({ sendKey, recvKey }) {
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
export function pairAdmitFor(pcId, code) {
  return createHash("sha256")
    .update(Buffer.from("bubbleplay-pair-admit-v1", "utf8"))
    .update(Buffer.from(pcId, "utf8"))
    .update(Buffer.from(code, "utf8"))
    .digest("base64url");
}

// The pair key: HKDF-SHA256 of the code, empty salt, info "bubbleplay-pair-v1".
export function pairKeyFor(code) {
  return Buffer.from(hkdfSync("sha256", Buffer.from(code, "utf8"), Buffer.alloc(0), Buffer.from("bubbleplay-pair-v1", "utf8"), 32));
}

// HMAC-SHA256(key, role || H).
export function pairMacFor(key, role, hash) {
  return createHmac("sha256", key).update(role, "utf8").update(hash).digest();
}

// The six-digit comparison: the first 4 bytes of SHA-256("bubbleplay-pair-sas-v1" || H), mod 10^6.
export function comparisonNumberFor(hash) {
  const digest = createHash("sha256").update("bubbleplay-pair-sas-v1", "utf8").update(hash).digest();
  return String(digest.readUInt32BE(0) % 1_000_000).padStart(6, "0");
}
