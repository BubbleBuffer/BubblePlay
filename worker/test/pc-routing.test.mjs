import test from "node:test";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { STREAM_LAB_COMPATIBILITY } from "../public/media.js";
import { ackMessageV4, parseFrameV4 } from "../public/media-v4.js";
import { decodeFilePacket, encodeData, encodeRequest } from "../public/file-transfer.js";
import {
  contract, PC, VIEWER, ALIAS_PC, RELAY_ORIGIN, freshNonce, tamper, signRelayProof, verifyWithFixtureKey, relayProofEncoding, unb64url,
} from "./helpers/dk-contract.mjs";
import {
  worker, relayEnv, browserCookie, connect, send, frames, framesOfType, close, registerHost, viewerConnect, viewerHello,
} from "./helpers/dk-fake-do.mjs";

const SESSION_TICKET_BYTES = 32;
const E_A = "a".repeat(32);
const E_B = "b".repeat(32);
const RUN = "123e4567-e89b-42d3-a456-426614174000";
const COMPAT = STREAM_LAB_COMPATIBILITY.id;

const sha256Hex = value => createHash("sha256").update(value).digest("hex");

function assertTicketShape(value) {
  assert.equal(typeof value, "string");
  assert.match(value, /^[A-Za-z0-9_-]{43}$/);
  assert.equal(unb64url(value).length, SESSION_TICKET_BYTES);
}

// A refused hello is one with no hello-ok, and a closed socket or an error frame.
function assertHostRefused(host, label) {
  assert.equal(host.reply, null, label);
  if (host.socket) assert.ok(host.socket.closed.length > 0 || frames(host.socket).some(frame => frame.type === "error"), label);
}

function assertSocketRefused(socket, label) {
  assert.equal(framesOfType(socket, "hello-ok").length, 0, label);
  assert.ok(socket.closed.length > 0 || frames(socket).some(frame => frame.type === "error"), label);
}

async function atOffset(offsetMs, operation) {
  const real = Date.now;
  Date.now = () => real() + offsetMs;
  try { return await operation(); } finally { Date.now = real; }
}

async function validate(env, pcId, body) {
  const response = await env.ROOM.instance(`pc-v1:${pcId}`).fetch(new Request("https://room.internal/__pc/validate", {
    method: "POST", body: JSON.stringify({ pc: pcId, ...body }),
  }));
  return response.status;
}

// The same v4 packet fixture the media tests validate against.
const V4_FIXTURE = JSON.parse(readFileSync(new URL("../../fixtures/wss-v4-contract.json", import.meta.url), "utf8"));

// Native Android handshake: the session Cookie, no Origin, no pc, no ticket.
const native = (env, path, cookie) => connect(env, path, { headers: { Cookie: cookie }, origin: null });
const hostBearer = env => ({ Authorization: `Bearer ${env.HOST_GATE_TOKEN}` });
// Binary frames enter the Room the way the Worker delivers them (send() only JSON-encodes text).
async function sendRaw(env, socket, raw) {
  const { roomName, namespace } = socket.attachment;
  return env[namespace].instance(roomName).webSocketMessage(socket, raw);
}
const hasFrame = (socket, bytes) => socket.messages.some(raw => Buffer.from(raw).equals(Buffer.from(bytes)));
const lastValidate = env => env.testLog.filter(entry => entry.binding === "ROOM" && entry.path === "/__pc/validate").at(-1);

async function onlineHost(env, identity = PC, extra = {}) {
  const host = await registerHost(env, { identity, token: env.TOKEN, ...extra });
  assert.equal(host.reply?.type, "hello-ok", "host registers");
  return host;
}

// Two cookie halves, to prove a viewer needs both.
function splitCookie(cookie) {
  const [gate, session] = cookie.split("; ");
  return { gate, session };
}

function legacyEnvWithHost(env) {
  return registerHost(env, { pc: null, noId: true, token: env.TOKEN });
}

test("fixture relay proofs: the independent signer reproduces the vectors and the verifier splits valid from invalid", async () => {
  for (const vector of contract.relayProof.vectors) {
    const encoding = relayProofEncoding({ role: vector.role, origin: vector.origin, nonce: vector.nonce, id: vector.id, public: vector.public });
    assert.equal(Buffer.from(encoding).toString("hex"), vector.encodingHex, vector.name);
    assert.equal(sha256Hex(encoding), vector.sha256Hex, vector.name);
    assert.equal(await verifyWithFixtureKey({ ...vector }), vector.expected === "valid", vector.name);
  }
  const host = contract.relayProof.vectors[0];
  const seed = contract.ids.find(entry => entry.id === host.id).seedHex;
  assert.equal(await signRelayProof({ seedHex: seed, origin: host.origin, nonce: host.nonce, id: host.id, public: host.public }), host.signature);
  // Replay: the fixture signature is for its own nonce, not a fresh one.
  assert.equal(await verifyWithFixtureKey({ ...host, nonce: freshNonce() }), false);
});

test("host registration: an unknown ID without TOKEN is refused and nothing is stored", async () => {
  const env = relayEnv();
  const host = await registerHost(env, { identity: PC });
  assertHostRefused(host, "unknown ID, no TOKEN");
  assert.equal(await env.gateState.storage.get(`pc:${PC.id}`), undefined);
});

test("host registration: TOKEN with a challenge and a fresh signature succeeds and returns a ticket and pc", async () => {
  const env = relayEnv();
  const host = await registerHost(env, { token: env.TOKEN });
  assert.equal(host.challenge?.type, "register-challenge");
  assert.match(host.challenge.nonce, /^[A-Za-z0-9_-]{43}$/);
  assert.equal(host.reply.type, "hello-ok");
  assert.equal(host.reply.pc, PC.id);
  assertTicketShape(host.reply.ticket);
  assert.equal((await env.gateState.storage.get(`pc:${PC.id}`)).public, PC.public);
});

test("host registration: a known ID registers without TOKEN, but a wrong explicit TOKEN fails even for a known ID", async () => {
  const env = relayEnv();
  await onlineHost(env);
  const again = await registerHost(env, {});
  assert.equal(again.reply?.pc, PC.id, "known ID, no token");
  const wrong = await registerHost(env, { token: "not-the-relay-token-0000000000000000" });
  assertHostRefused(wrong, "known ID, wrong TOKEN");
});

test("a registration challenge cannot substitute a rotated TOKEN for the token supplied in hello", async () => {
  const env = relayEnv();
  const pending = await registerHost(env, { token: env.TOKEN, sign: false });
  env.TOKEN = "rotated-registration-token-000000000000000000000000";
  const signature = await signRelayProof({ seedHex: PC.seedHex, origin: RELAY_ORIGIN,
    nonce: pending.challenge.nonce, id: PC.id, public: PC.public });
  await send(env, pending.socket, { type: "register-proof", signature });
  assertSocketRefused(pending.socket, "TOKEN changed during the challenge");
  assert.equal(await env.gateState.storage.get(`pc:${PC.id}`), undefined);
});

test("host registration: an ID that does not derive from the public key fails, and hello.id must equal the URL pc canonically", async () => {
  const env = relayEnv();
  assertHostRefused(await registerHost(env, { token: env.TOKEN, helloPublic: VIEWER.public }), "id/public mismatch");
  assertHostRefused(await registerHost(env, { token: env.TOKEN, pc: ALIAS_PC.id, identity: PC, helloId: PC.id }), "hello.id differs from URL pc");
  // Aliases are accepted and canonicalized: the URL and hello may spell the same ID differently.
  const alias = await registerHost(env, { identity: ALIAS_PC, pc: "hnobw2w6oqqilm47", helloId: "hnob-w2w6-oqqi-lm47", token: env.TOKEN });
  assert.equal(alias.reply?.pc, ALIAS_PC.id);
  assert.equal(alias.socket.attachment.roomName, `pc-v1:${ALIAS_PC.id}`);
});

test("host registration: a hello with no ID on a pc URL fails, and an ID hello on the legacy URL fails", async () => {
  const env = relayEnv();
  assertHostRefused(await registerHost(env, { noId: true, token: env.TOKEN }), "no id on pc URL");
  assertHostRefused(await registerHost(env, { pc: null, token: env.TOKEN }), "id hello on legacy URL");
  const legacy = await legacyEnvWithHost(env);
  assert.equal(legacy.reply.type, "hello-ok");
  assert.equal(legacy.reply.pc, undefined, "legacy hello-ok names no pc");
  assertTicketShape(legacy.reply.ticket);
  assert.equal(legacy.socket.attachment.roomName, "room-v3");
});

test("host registration: a wrong fixture signature and a replayed signature both fail against a fresh nonce", async () => {
  const env = relayEnv();
  await onlineHost(env);
  const fixtureSignature = contract.relayProof.messages.registerProof.signature;
  assertHostRefused(await registerHost(env, { token: env.TOKEN, signature: fixtureSignature }), "fixture signature");
  const replayed = await signRelayProof({ seedHex: PC.seedHex, origin: RELAY_ORIGIN, nonce: freshNonce(), id: PC.id, public: PC.public });
  assertHostRefused(await registerHost(env, { signature: replayed }), "signature for another nonce");

  // Control: the same flow with a valid signature over the issued nonce succeeds; a bit-flipped one fails.
  const first = await registerHost(env, { sign: false });
  const valid = await signRelayProof({ seedHex: PC.seedHex, origin: RELAY_ORIGIN, nonce: first.challenge.nonce, id: PC.id, public: PC.public });
  await send(env, first.socket, { type: "register-proof", signature: tamper(valid) });
  assert.equal(framesOfType(first.socket, "hello-ok").length, 0, "tampered signature");
  const second = await registerHost(env, { sign: false });
  await send(env, second.socket, { type: "register-proof", signature: valid.replace(/^./, c => (c === "A" ? "B" : "A")) });
  assert.equal(framesOfType(second.socket, "hello-ok").length, 0, "altered first character fails");
});

test("two PCs and the legacy host get separate rooms, epochs and offer delivery", async () => {
  const env = relayEnv();
  const hostA = await onlineHost(env, PC);
  const hostB = await onlineHost(env, ALIAS_PC);
  const legacy = await legacyEnvWithHost(env);
  const cookie = await browserCookie(env);
  const viewerA = await viewerConnect(env, `/ws?pc=${PC.id}`, cookie);
  const viewerB = await viewerConnect(env, `/ws?pc=${ALIAS_PC.id}`, cookie);
  const helloA = await viewerHello(env, viewerA.socket);
  const helloB = await viewerHello(env, viewerB.socket);
  assert.equal(helloA.pc, PC.id);
  assert.equal(helloB.pc, ALIAS_PC.id);
  assert.match(helloA.mediaEpoch, /^[a-f0-9]{32}$/);
  assert.notEqual(helloA.mediaEpoch, helloB.mediaEpoch);
  assert.equal(viewerA.socket.attachment.roomName, `pc-v1:${PC.id}`);
  assert.equal(viewerB.socket.attachment.roomName, `pc-v1:${ALIAS_PC.id}`);
  assert.notEqual(env.ROOM.instance(`pc-v1:${PC.id}`), env.ROOM.instance(`pc-v1:${ALIAS_PC.id}`));
  assert.notEqual(env.ROOM.instance(`pc-v1:${PC.id}`), env.ROOM.instance("room-v3"));

  await send(env, viewerB.socket, { type: "offer", sdp: "for-b" });
  assert.equal(framesOfType(hostB.socket, "offer").length, 1, "PC B host gets its offer");
  assert.equal(framesOfType(hostA.socket, "offer").length, 0, "PC A host gets nothing");
  assert.equal(framesOfType(legacy.socket, "offer").length, 0, "legacy host gets nothing");
});

test("a viewer offer buffered while its PC host is away stays in that PC's room", async () => {
  const env = relayEnv();
  await onlineHost(env, PC);
  const hostB = await onlineHost(env, ALIAS_PC);
  await close(env, hostB.socket, 1000);
  const cookie = await browserCookie(env);
  const viewer = await viewerConnect(env, `/ws?pc=${ALIAS_PC.id}`, cookie);
  await viewerHello(env, viewer.socket);
  await send(env, viewer.socket, { type: "offer", sdp: "waiting" });
  assert.equal((await env.ROOM.instance(`pc-v1:${ALIAS_PC.id}`).state.storage.get("pendingToHost")).length, 1);
  assert.equal(await env.ROOM.instance(`pc-v1:${PC.id}`).state.storage.get("pendingToHost"), undefined);
  assert.equal(await env.ROOM.instance("room-v3").state.storage.get("pendingToHost"), undefined);
});

test("viewer /ws resolves an online PC, then online legacy, then the sole offline listed PC, then empty legacy", async () => {
  const env = relayEnv();
  const hostA = await onlineHost(env, PC);
  const cookie = await browserCookie(env);
  const resolved = await viewerConnect(env, "/ws", cookie);
  assert.equal(resolved.socket.attachment.roomName, `pc-v1:${PC.id}`);
  assert.equal((await viewerHello(env, resolved.socket)).pc, PC.id);

  await close(env, hostA.socket, 1000);
  const legacy = await legacyEnvWithHost(env);
  const fallback = await viewerConnect(env, "/ws", cookie);
  assert.equal(fallback.socket.attachment.roomName, "room-v3");
  assert.equal((await viewerHello(env, fallback.socket)).pc, undefined);

  await close(env, legacy.socket, 1000);
  const soleOffline = await viewerConnect(env, "/ws", cookie);
  assert.equal(soleOffline.socket.attachment.roomName, `pc-v1:${PC.id}`);
  await env.GATE.get().fetch(new Request(`${RELAY_ORIGIN}/__gate/manage/pcs/${PC.id}`, {
    method: "DELETE", headers: { Authorization: `Bearer ${env.GATE_ADMIN_TOKEN}` },
  }));
  const empty = await viewerConnect(env, "/ws", cookie);
  assert.equal(empty.socket.attachment.roomName, "room-v3");
});

test("/__pc/online reports the PC host's presence and never reaches the public Worker", async () => {
  const env = relayEnv();
  const host = await onlineHost(env, PC);
  const online = () => env.ROOM.instance(`pc-v1:${PC.id}`).fetch(new Request("https://room.internal/__pc/online"));
  assert.deepEqual(await (await online()).json(), { online: true });
  await close(env, host.socket, 1000);
  assert.deepEqual(await (await online()).json(), { online: false });
  for (const path of ["/__pc/online", "/__pc/validate", "/__pc/retire", "/__pc/delete"]) {
    const response = await worker.fetch(new Request(`${RELAY_ORIGIN}${path}`, { method: "POST" }), env);
    assert.equal(response.status, 404, path);
  }
});

test("invalid pc is a 404 at upgrade on every socket path, including the lab twin", async () => {
  const env = relayEnv();
  const cookie = await browserCookie(env);
  const bearer = { Authorization: `Bearer ${env.HOST_GATE_TOKEN}` };
  const cases = [
    ["/ws?pc=bogus", { Cookie: cookie }],
    ["/ws?pc=77VH&pc=77VH-7M56-8GJK-Y12J", { Cookie: cookie }],
    [`/media-ws?pc=bogus&epoch=${E_A}`, { Cookie: cookie }],
    [`/file-ws?pc=bogus&epoch=${E_A}`, { Cookie: cookie }],
    ["/host-ws?pc=bogus", bearer],
    [`/lab-host-ws?pc=bogus&run=${RUN}&compat=${COMPAT}`, bearer],
    [`/lab-ws?pc=bogus&run=${RUN}&compat=${COMPAT}`, { Cookie: cookie }],
  ];
  for (const [path, headers] of cases) {
    const result = await connect(env, path, { headers, origin: path.includes("host") ? null : RELAY_ORIGIN });
    assert.equal(result.status, 404, path);
  }
});

test("hello-ok shape: ticket on every signaling role, pc only for ID rooms, version probe unchanged", async () => {
  const env = relayEnv();
  const host = await onlineHost(env, PC);
  assertTicketShape(host.reply.ticket);
  assert.equal(host.reply.pc, PC.id);

  const cookie = await browserCookie(env);
  const viewer = await viewerConnect(env, `/ws?pc=${PC.id}`, cookie);
  const hello = await viewerHello(env, viewer.socket);
  assertTicketShape(hello.ticket);
  assert.equal(hello.pc, PC.id);
  assert.ok(Array.isArray(hello.iceServers));
  assert.match(hello.mediaEpoch, /^[a-f0-9]{32}$/);

  const legacy = await legacyEnvWithHost(env);
  assertTicketShape(legacy.reply.ticket);
  assert.equal("pc" in legacy.reply, false);

  const probe = await connect(env, "/host-version-ws", { headers: { Authorization: `Bearer ${env.HOST_GATE_TOKEN}` }, origin: null });
  await send(env, probe.socket, { type: "hello", role: "version-probe", token: env.TOKEN });
  const probeReply = frames(probe.socket).find(frame => frame.type === "hello-ok");
  assert.ok(probeReply, "version probe answered");
  assert.equal("ticket" in probeReply, false, "version probe is not a signaling session");
  assert.equal("pc" in probeReply, false);
});

test("media and file sockets with ?pc&epoch use the existing media-v4 and file-v1 room names", async () => {
  const env = relayEnv();
  await onlineHost(env, PC);
  const cookie = await browserCookie(env);
  const viewer = await viewerConnect(env, `/ws?pc=${PC.id}`, cookie);
  const hello = await viewerHello(env, viewer.socket);
  const media = await viewerConnect(env, `/media-ws?pc=${PC.id}&epoch=${hello.mediaEpoch}`, cookie);
  assert.equal(media.socket.attachment.roomName, `media-v4:${hello.mediaEpoch}`);
  assert.equal(media.socket.attachment.namespace, "MEDIA_ROOM");
  await send(env, media.socket, { type: "hello", role: "media-client", ticket: hello.ticket });
  assert.equal(framesOfType(media.socket, "hello-ok").length, 1);
  const file = await viewerConnect(env, `/file-ws?pc=${PC.id}&epoch=${hello.mediaEpoch}`, cookie);
  assert.equal(file.socket.attachment.roomName, `file-v1:${hello.mediaEpoch}`);
  assert.equal(file.socket.attachment.namespace, "MEDIA_ROOM");
});

test("host signaling needs the bearer; host media and file need the bearer or a valid host signaling ticket", async () => {
  const env = relayEnv();
  const host = await onlineHost(env, PC);
  const bare = await connect(env, `/host-ws?pc=${PC.id}`, { origin: null });
  assert.equal(bare.status, 404, "signaling without bearer, even with a ticket");

  const hostMedia = `/host-media-ws?pc=${PC.id}&epoch=${E_A}`;
  const withBearer = await connect(env, hostMedia, { headers: { Authorization: `Bearer ${env.HOST_GATE_TOKEN}` }, origin: null });
  await send(env, withBearer.socket, { type: "hello", role: "media-host" });
  assert.equal(framesOfType(withBearer.socket, "hello-ok").length, 1, "bearer, no ticket: old behavior");

  const noBearerNoTicket = await connect(env, hostMedia, { origin: null });
  await send(env, noBearerNoTicket.socket, { type: "hello", role: "media-host" });
  assertSocketRefused(noBearerNoTicket.socket, "no bearer, no ticket");

  const bogus = await connect(env, hostMedia, { origin: null });
  await send(env, bogus.socket, { type: "hello", role: "media-host", ticket: "x".repeat(43) });
  assertSocketRefused(bogus.socket, "no bearer, bogus ticket");

  const withTicket = await connect(env, hostMedia, { origin: null });
  await send(env, withTicket.socket, { type: "hello", role: "media-host", ticket: host.reply.ticket });
  assert.equal(framesOfType(withTicket.socket, "hello-ok").length, 1, "no bearer, valid host signaling ticket");

  const fileHost = await connect(env, `/host-file-ws?pc=${PC.id}&epoch=${E_A}`, { origin: null });
  await send(env, fileHost.socket, { type: "hello", role: "file-host", ticket: host.reply.ticket });
  assert.equal(framesOfType(fileHost.socket, "hello-ok").length, 1, "file host with ticket");
});

test("viewer media needs both cookies, and an optional ticket must match room, epoch and an open issuing socket", async () => {
  const env = relayEnv();
  await onlineHost(env, PC);
  const other = await onlineHost(env, ALIAS_PC);
  const cookie = await browserCookie(env);
  const { gate, session } = splitCookie(cookie);
  const viewer = await viewerConnect(env, `/ws?pc=${PC.id}`, cookie);
  const hello = await viewerHello(env, viewer.socket);
  const path = `/media-ws?pc=${PC.id}&epoch=${hello.mediaEpoch}`;

  for (const partial of [gate, session]) {
    const half = await connect(env, path, { headers: { Cookie: partial } });
    assert.equal(half.status, 404, `only ${partial.split("=")[0]}`);
  }
  const plain = await viewerConnect(env, path, cookie);
  await send(env, plain.socket, { type: "hello", role: "media-client" });
  assert.equal(framesOfType(plain.socket, "hello-ok").length, 1, "no ticket is allowed for a viewer");

  const otherTicket = await viewerConnect(env, path, cookie);
  await send(env, otherTicket.socket, { type: "hello", role: "media-client", ticket: other.reply.ticket });
  assertSocketRefused(otherTicket.socket, "ticket of another PC");

  const bogus = await viewerConnect(env, path, cookie);
  await send(env, bogus.socket, { type: "hello", role: "media-client", ticket: "y".repeat(43) });
  assertSocketRefused(bogus.socket, "bogus ticket");

  const good = await viewerConnect(env, path, cookie);
  await send(env, good.socket, { type: "hello", role: "media-client", ticket: hello.ticket });
  assert.equal(framesOfType(good.socket, "hello-ok").length, 1, "matching ticket");

  // A ticket for one epoch does not open another.
  const otherEpoch = await viewerConnect(env, `/media-ws?pc=${PC.id}&epoch=${E_B}`, cookie);
  await send(env, otherEpoch.socket, { type: "hello", role: "media-client", ticket: hello.ticket });
  assertSocketRefused(otherEpoch.socket, "ticket for another epoch");
});

test("a viewer ticket validates for at most 12 hours and only while its issuing signaling socket is open", async () => {
  const env = relayEnv();
  await onlineHost(env, PC);
  const cookie = await browserCookie(env);
  const viewer = await viewerConnect(env, `/ws?pc=${PC.id}`, cookie);
  const hello = await viewerHello(env, viewer.socket);
  const check = () => validate(env, PC.id, { role: "viewer", epoch: hello.mediaEpoch, ticket: hello.ticket, requireTicket: false });

  assert.equal(await check(), 204, "fresh");
  assert.equal(await atOffset(11 * 3600_000, check), 204, "11 hours");
  assert.equal(await atOffset(12 * 3600_000 + 60_000, check), 404, "past 12 hours");
  assert.equal(await validate(env, PC.id, { role: "viewer", epoch: E_B, ticket: hello.ticket, requireTicket: false }), 404, "wrong epoch");
  assert.equal(await validate(env, ALIAS_PC.id, { role: "viewer", epoch: hello.mediaEpoch, ticket: hello.ticket, requireTicket: false }), 404, "wrong room");

  await close(env, viewer.socket, 1000);
  assert.equal(await check(), 404, "issuing socket closed");
});

test("tickets are stored only as hashes in socket attachments and never in room storage", async () => {
  const env = relayEnv();
  const host = await onlineHost(env, PC);
  const cookie = await browserCookie(env);
  const viewer = await viewerConnect(env, `/ws?pc=${PC.id}`, cookie);
  const hello = await viewerHello(env, viewer.socket);
  const attachments = JSON.stringify([host.socket.attachment, viewer.socket.attachment]);
  assert.ok(attachments.includes(sha256Hex(host.reply.ticket)), "host attachment holds the host ticket hash");
  assert.ok(attachments.includes(sha256Hex(hello.ticket)), "viewer attachment holds the viewer ticket hash");
  assert.equal(attachments.includes(hello.ticket), false);
  assert.equal(attachments.includes(host.reply.ticket), false);
  const stored = JSON.stringify([...env.ROOM.instance(`pc-v1:${PC.id}`).state.storage.values.entries()]);
  assert.equal(stored.includes(hello.ticket), false);
  assert.equal(stored.includes(host.reply.ticket), false);
});

test("a media or file hello asks the issuing Room exactly once, and that check is the only Room validate call", async () => {
  const env = relayEnv();
  await onlineHost(env, PC);
  const cookie = await browserCookie(env);
  const viewer = await viewerConnect(env, `/ws?pc=${PC.id}`, cookie);
  const hello = await viewerHello(env, viewer.socket);
  const before = env.testLog.length;
  const media = await viewerConnect(env, `/media-ws?pc=${PC.id}&epoch=${hello.mediaEpoch}`, cookie);
  await send(env, media.socket, { type: "hello", role: "media-client", ticket: hello.ticket });
  const validations = env.testLog.slice(before).filter(entry => entry.binding === "ROOM" && entry.path === "/__pc/validate");
  assert.equal(validations.length, 1);
  assert.equal(validations[0].name, `pc-v1:${PC.id}`);
  assert.deepEqual({ ...validations[0].body }, { pc: PC.id, role: "viewer", epoch: hello.mediaEpoch, ticket: hello.ticket, requireTicket: false });
});

test("closing a viewer's signaling socket invalidates its ticket and retires only that PC's media and file sockets", async () => {
  const env = relayEnv();
  await onlineHost(env, PC);
  await onlineHost(env, ALIAS_PC);
  const cookie = await browserCookie(env);
  const viewerA = await viewerConnect(env, `/ws?pc=${PC.id}`, cookie);
  const helloA = await viewerHello(env, viewerA.socket);
  const viewerB = await viewerConnect(env, `/ws?pc=${ALIAS_PC.id}`, cookie);
  const helloB = await viewerHello(env, viewerB.socket);
  const mediaA = await viewerConnect(env, `/media-ws?pc=${PC.id}&epoch=${helloA.mediaEpoch}`, cookie);
  await send(env, mediaA.socket, { type: "hello", role: "media-client", ticket: helloA.ticket });
  const fileA = await viewerConnect(env, `/file-ws?pc=${PC.id}&epoch=${helloA.mediaEpoch}`, cookie);
  await send(env, fileA.socket, { type: "hello", role: "file-client", ticket: helloA.ticket });
  const mediaB = await viewerConnect(env, `/media-ws?pc=${ALIAS_PC.id}&epoch=${helloB.mediaEpoch}`, cookie);
  await send(env, mediaB.socket, { type: "hello", role: "media-client", ticket: helloB.ticket });

  // A retire aimed at PC A's epoch closes nothing belonging to PC B, even on the same epoch.
  const retire = (pc, epoch) => env.MEDIA_ROOM.instance(`media-v4:${epoch}`).fetch(new Request("https://room.internal/__pc/retire", {
    method: "POST", body: JSON.stringify({ pc, epoch }),
  }));
  await retire(ALIAS_PC.id, helloA.mediaEpoch);
  assert.equal(mediaA.socket.closed.length, 0, "a PC B retire leaves PC A's media alone");
  await retire(PC.id, helloB.mediaEpoch);
  assert.equal(mediaB.socket.closed.length, 0, "a PC A retire leaves PC B's media alone");

  await close(env, viewerA.socket, 1000);
  assert.equal(await validate(env, PC.id, { role: "viewer", epoch: helloA.mediaEpoch, ticket: helloA.ticket, requireTicket: false }), 404);
  assert.ok(mediaA.socket.closed.length > 0, "leaving retires PC A's media socket");
  assert.ok(fileA.socket.closed.length > 0, "leaving retires PC A's file socket");
  assert.equal(mediaB.socket.closed.length, 0, "PC B's media survives PC A's viewer leaving");
  assert.equal(viewerB.socket.closed.length, 0);
});

test("admin deletion closes the PC's signaling and host media and file sockets, and the PC then needs TOKEN again", async () => {
  const env = relayEnv();
  const host = await onlineHost(env, PC);
  const bearer = { Authorization: `Bearer ${env.HOST_GATE_TOKEN}` };
  const hostMedia = await connect(env, `/host-media-ws?pc=${PC.id}&epoch=${E_A}`, { headers: bearer, origin: null });
  await send(env, hostMedia.socket, { type: "hello", role: "media-host" });
  const hostFile = await connect(env, `/host-file-ws?pc=${PC.id}&epoch=${E_A}`, { headers: bearer, origin: null });
  await send(env, hostFile.socket, { type: "hello", role: "file-host" });

  const deleted = await env.GATE.get().fetch(new Request(`${RELAY_ORIGIN}/__gate/manage/pcs/${PC.id}`, {
    method: "DELETE", headers: { Authorization: `Bearer ${env.GATE_ADMIN_TOKEN}`, Origin: RELAY_ORIGIN },
  }));
  assert.equal(deleted.status, 204);
  assert.ok(host.socket.closed.length > 0, "signaling host closed");
  assert.ok(hostMedia.socket.closed.length > 0, "host media closed");
  assert.ok(hostFile.socket.closed.length > 0, "host file closed");
  assert.equal(await env.gateState.storage.get(`pc:${PC.id}`), undefined);

  assertHostRefused(await registerHost(env, {}), "deleted PC without TOKEN");
  const back = await registerHost(env, { token: env.TOKEN });
  assert.equal(back.reply?.pc, PC.id, "deleted PC re-registers with TOKEN");
});

test("the lab twin /lab-host-ws requires a pc for an ID hello, like /host-ws", async () => {
  const env = relayEnv();
  const labPath = "/lab-host-ws";
  const labParams = [`run=${RUN}`, `compat=${COMPAT}`];
  assertHostRefused(await registerHost(env, { path: labPath, pc: null, params: labParams, token: env.TOKEN }), "lab ID hello, no pc");
  const lab = await registerHost(env, { path: labPath, params: labParams, token: env.TOKEN });
  assert.equal(lab.reply?.pc, PC.id, "lab ID hello with pc");
});

// The old Android client: /ws hello {role:"client"}, then /media-ws and /file-ws by
// mediaEpoch with no pc and no ticket. All three must reach the ID host's PC.
test("an old viewer with no pc reaches the ID host on /ws, /media-ws and /file-ws", async () => {
  const env = relayEnv();
  await onlineHost(env, PC);
  const cookie = await browserCookie(env);
  const viewer = await viewerConnect(env, "/ws", cookie);
  assert.equal(viewer.socket.attachment.roomName, `pc-v1:${PC.id}`);
  const hello = await viewerHello(env, viewer.socket);
  assert.equal(hello.pc, PC.id);

  const media = await viewerConnect(env, `/media-ws?epoch=${hello.mediaEpoch}`, cookie);
  assert.equal(media.status, 101);
  assert.equal(media.socket.attachment.pc, PC.id, "media binds to the PC /ws resolved");
  assert.equal(media.socket.attachment.roomName, `media-v4:${hello.mediaEpoch}`, "media room is still epoch-keyed");
  await send(env, media.socket, { type: "hello", role: "media-client" });
  assert.equal(framesOfType(media.socket, "hello-ok").length, 1, "no ticket, bound by the viewer's epoch");

  const file = await viewerConnect(env, `/file-ws?epoch=${hello.mediaEpoch}`, cookie);
  assert.equal(file.status, 101);
  assert.equal(file.socket.attachment.pc, PC.id, "file binds to the PC /ws resolved");
  assert.equal(file.socket.attachment.roomName, `file-v1:${hello.mediaEpoch}`);
  await send(env, file.socket, { type: "hello", role: "file-client" });
  assert.equal(framesOfType(file.socket, "hello-ok").length, 1, "file: no ticket, bound by epoch");

  const validations = env.testLog.filter(entry => entry.binding === "ROOM" && entry.path === "/__pc/validate");
  assert.equal(validations.length, 2, "one validate per auxiliary hello");
  assert.ok(validations.every(entry => entry.name === `pc-v1:${PC.id}` && entry.body.defaultPc === true));

  // The epoch check is the same one the ticket path uses: the wrong epoch and the wrong PC are refused.
  assert.equal(await validate(env, PC.id, { role: "viewer", epoch: hello.mediaEpoch, defaultPc: true, requireTicket: false }), 204);
  assert.equal(await validate(env, PC.id, { role: "viewer", epoch: E_B, defaultPc: true, requireTicket: false }), 404, "wrong epoch");
  assert.equal(await validate(env, ALIAS_PC.id, { role: "viewer", epoch: hello.mediaEpoch, defaultPc: true, requireTicket: false }), 404, "wrong PC");

  // Leaving retires the defaulted auxiliary sockets, because they carry the PC too.
  await close(env, viewer.socket, 1000);
  assert.ok(media.socket.closed.length > 0, "media retired with its signaling viewer");
  assert.ok(file.socket.closed.length > 0, "file retired with its signaling viewer");
});

test("a pc-less media or file upgrade that resolves a PC the viewer is not on is refused at hello", async () => {
  const env = relayEnv();
  const hostA = await onlineHost(env, PC);
  const cookie = await browserCookie(env);
  const viewer = await viewerConnect(env, "/ws", cookie);
  const hello = await viewerHello(env, viewer.socket);
  // Upgrades made while PC A is the default bind to A, and the viewer is on A.
  const media = await viewerConnect(env, `/media-ws?epoch=${hello.mediaEpoch}`, cookie);
  const file = await viewerConnect(env, `/file-ws?epoch=${hello.mediaEpoch}`, cookie);
  assert.equal(media.socket.attachment.pc, PC.id);
  assert.equal(file.socket.attachment.pc, PC.id);

  // The default moves to B. The viewer's signaling socket is still on A.
  await close(env, hostA.socket, 1000);
  await onlineHost(env, ALIAS_PC);
  // A new upgrade resolves B, which the viewer is not on. Upgrade succeeds; hello is refused.
  const moved = await viewerConnect(env, `/media-ws?epoch=${hello.mediaEpoch}`, cookie);
  assert.equal(moved.status, 101, "the upgrade itself resolves");
  assert.equal(moved.socket.attachment.pc, ALIAS_PC.id);
  await send(env, moved.socket, { type: "hello", role: "media-client" });
  assertSocketRefused(moved.socket, "media upgrade for B, viewer on A");
  const movedFile = await viewerConnect(env, `/file-ws?epoch=${hello.mediaEpoch}`, cookie);
  assert.equal(movedFile.socket.attachment.pc, ALIAS_PC.id);
  await send(env, movedFile.socket, { type: "hello", role: "file-client" });
  assertSocketRefused(movedFile.socket, "file upgrade for B, viewer on A");

  // The upgrade made under A still belongs to the viewer on A, so its hello is accepted.
  await send(env, media.socket, { type: "hello", role: "media-client" });
  assert.equal(framesOfType(media.socket, "hello-ok").length, 1, "A-bound media, viewer on A");
  await send(env, file.socket, { type: "hello", role: "file-client" });
  assert.equal(framesOfType(file.socket, "hello-ok").length, 1, "A-bound file, viewer on A");

  // Positive control: the same viewer re-hellos on /ws under B, gets a new epoch, and its defaulted media works.
  const viewerB = await viewerConnect(env, "/ws", cookie);
  const helloB = await viewerHello(env, viewerB.socket);
  assert.equal(helloB.pc, ALIAS_PC.id);
  const mediaB = await viewerConnect(env, `/media-ws?epoch=${helloB.mediaEpoch}`, cookie);
  assert.equal(mediaB.socket.attachment.pc, ALIAS_PC.id);
  await send(env, mediaB.socket, { type: "hello", role: "media-client" });
  assert.equal(framesOfType(mediaB.socket, "hello-ok").length, 1, "re-hello on B: media accepted");
});

test("the lab viewer's pc-less media stays on its run's signaling room and never resolves the default PC", async () => {
  const env = relayEnv();
  await onlineHost(env, PC);
  const cookie = await browserCookie(env);
  const labQuery = `run=${RUN}&compat=${COMPAT}`;
  const lab = await viewerConnect(env, `/lab-ws?${labQuery}`, cookie);
  assert.equal(lab.socket.attachment.roomName, `lab-signal-v1:${RUN}`);
  const labHello = await viewerHello(env, lab.socket);
  assert.equal(labHello.pc, undefined, "lab signaling names no PC");

  const media = await viewerConnect(env, `/lab-media-ws?${labQuery}&epoch=${labHello.mediaEpoch}`, cookie);
  assert.equal(media.status, 101);
  assert.equal(media.socket.attachment.pc, null, "no default PC on the lab twin");
  assert.equal(media.socket.attachment.defaultPc, true, "pc-less lab media is marked for the epoch check");
  await send(env, media.socket, { type: "hello", role: "media-client" });
  assert.equal(framesOfType(media.socket, "hello-ok").length, 1);
  const validations = env.testLog.filter(entry => entry.binding === "ROOM" && entry.path === "/__pc/validate");
  assert.equal(validations.at(-1).name, `lab-signal-v1:${RUN}`, "validated against the run's signaling room");
  assert.equal(validations.at(-1).body.defaultPc, true);
});

test("explicit-pc media without a ticket keeps its old admission: the viewer's epoch is not checked", async () => {
  const env = relayEnv();
  await onlineHost(env, PC);
  await onlineHost(env, ALIAS_PC);
  const cookie = await browserCookie(env);
  const viewer = await viewerConnect(env, `/ws?pc=${PC.id}`, cookie);
  await viewerHello(env, viewer.socket);
  // Unchanged from before the default-PC routing: an explicit pc and a stale epoch are admitted without a ticket.
  const explicit = await viewerConnect(env, `/media-ws?pc=${ALIAS_PC.id}&epoch=${E_B}`, cookie);
  await send(env, explicit.socket, { type: "hello", role: "media-client" });
  assert.equal(framesOfType(explicit.socket, "hello-ok").length, 1, "explicit pc, no ticket, epoch not checked");
  const validations = env.testLog.filter(entry => entry.binding === "ROOM" && entry.path === "/__pc/validate");
  assert.equal("defaultPc" in validations.at(-1).body, false, "explicit pc is not marked as defaulted");
});

test("native Android viewer with no pc joins the ID host: offer, media frame, ACK and file packets all reach it", async () => {
  const env = relayEnv();
  const host = await onlineHost(env, PC);
  const cookie = await browserCookie(env);

  // Signaling: the old client's hello, then its offer reaches the ID host's signaling socket.
  const viewer = await native(env, "/ws", cookie);
  assert.equal(viewer.status, 101);
  const hello = await viewerHello(env, viewer.socket);
  assert.equal(hello.pc, PC.id);
  const epoch = hello.mediaEpoch;
  await send(env, viewer.socket, JSON.stringify({ type: "offer", sdp: "v=0 native-android" }));
  assert.deepEqual(framesOfType(host.socket, "offer").map(frame => frame.sdp), ["v=0 native-android"]);

  // Media: the host joins the same epoch; the viewer's pc-less media is admitted and bound to the PC.
  const mediaHost = await connect(env, `/host-media-ws?pc=${PC.id}&epoch=${epoch}`, { headers: hostBearer(env), origin: null });
  await send(env, mediaHost.socket, { type: "hello", role: "media-host" });
  assert.equal(framesOfType(mediaHost.socket, "hello-ok").length, 1, "host media joins the epoch");
  const media = await native(env, `/media-ws?epoch=${epoch}`, cookie);
  assert.equal(media.status, 101);
  assert.equal(media.socket.attachment.pc, PC.id);
  await send(env, media.socket, { type: "hello", role: "media-client" });
  assert.equal(framesOfType(media.socket, "hello-ok").length, 1, "viewer media admitted without a ticket");

  // A fixture packet is validated before it is sent, and reaches the viewer byte for byte.
  const fixture = V4_FIXTURE.packets.find(packet => packet.accepted);
  const packet = Buffer.from(fixture.hex, "hex");
  assert.ok(parseFrameV4(packet), "fixture packet parses");
  await sendRaw(env, mediaHost.socket, packet);
  assert.ok(hasFrame(media.socket, packet), "frame reached the viewer");

  // The viewer's ACK reaches the ID host's media socket.
  await send(env, media.socket, ackMessageV4({ generation: fixture.generation, seq: fixture.seq, recvTsMs: 12.5, decodedSeq: fixture.seq }));
  assert.deepEqual(framesOfType(mediaHost.socket, "ack").map(frame => [frame.generation, frame.seq]), [[fixture.generation, fixture.seq]]);

  // File: the host joins the epoch; the client's REQUEST and the host's DATA each reach the other side.
  const fileHost = await connect(env, `/host-file-ws?pc=${PC.id}&epoch=${epoch}`, { headers: hostBearer(env), origin: null });
  await send(env, fileHost.socket, { type: "hello", role: "file-host" });
  assert.equal(framesOfType(fileHost.socket, "hello-ok").length, 1, "host file joins the epoch");
  const file = await native(env, `/file-ws?epoch=${epoch}`, cookie);
  assert.equal(file.status, 101);
  assert.equal(file.socket.attachment.pc, PC.id);
  await send(env, file.socket, { type: "hello", role: "file-client" });
  assert.equal(framesOfType(file.socket, "hello-ok").length, 1, "viewer file admitted without a ticket");
  const request = encodeRequest(7, 7, 7);
  const data = encodeData(7, 0, new Uint8Array([1, 2, 3, 4]));
  assert.equal(decodeFilePacket(request).ok, true);
  assert.equal(decodeFilePacket(data).ok, true);
  await sendRaw(env, file.socket, request);
  assert.ok(hasFrame(fileHost.socket, request), "request reached the file host");
  await sendRaw(env, fileHost.socket, data);
  assert.ok(hasFrame(file.socket, data), "data reached the file client");
});

test("with PC A still online and B registered later, a pc-less media or file socket carrying A's epoch is refused", async () => {
  const env = relayEnv();
  await onlineHost(env, PC);
  const cookie = await browserCookie(env);
  const viewerA = await native(env, "/ws", cookie);
  const helloA = await viewerHello(env, viewerA.socket);
  assert.equal(helloA.pc, PC.id);

  // B registers at a later time, so it is the newest online PC and the default. A stays online.
  await atOffset(60_000, () => onlineHost(env, ALIAS_PC));
  const viewerB = await native(env, "/ws", cookie);
  assert.equal(viewerB.socket.attachment.roomName, `pc-v1:${ALIAS_PC.id}`);
  const helloB = await viewerHello(env, viewerB.socket);
  assert.equal(helloB.pc, ALIAS_PC.id);
  assert.notEqual(helloB.mediaEpoch, helloA.mediaEpoch);

  // A's epoch, pc-less: both sockets bind to B, where the viewer has another epoch. Both are refused.
  const mediaA = await native(env, `/media-ws?epoch=${helloA.mediaEpoch}`, cookie);
  assert.equal(mediaA.socket.attachment.pc, ALIAS_PC.id);
  await send(env, mediaA.socket, { type: "hello", role: "media-client" });
  assertSocketRefused(mediaA.socket, "media with A's epoch under B");
  assert.equal(lastValidate(env).name, `pc-v1:${ALIAS_PC.id}`, "checked against B's signaling room");
  const fileA = await native(env, `/file-ws?epoch=${helloA.mediaEpoch}`, cookie);
  assert.equal(fileA.socket.attachment.pc, ALIAS_PC.id);
  await send(env, fileA.socket, { type: "hello", role: "file-client" });
  assertSocketRefused(fileA.socket, "file with A's epoch under B");

  // Positive control: B's own epoch is accepted on both sockets.
  const mediaB = await native(env, `/media-ws?epoch=${helloB.mediaEpoch}`, cookie);
  await send(env, mediaB.socket, { type: "hello", role: "media-client" });
  assert.equal(framesOfType(mediaB.socket, "hello-ok").length, 1, "media with B's epoch");
  const fileB = await native(env, `/file-ws?epoch=${helloB.mediaEpoch}`, cookie);
  await send(env, fileB.socket, { type: "hello", role: "file-client" });
  assert.equal(framesOfType(fileB.socket, "hello-ok").length, 1, "file with B's epoch");
  assert.equal(viewerA.socket.closed.length, 0, "A's signaling viewer is untouched");
});

test("a viewer's signaling replacement between an upgrade and its hello refuses the replaced epoch", async () => {
  const env = relayEnv();
  await onlineHost(env, PC);
  const cookie = await browserCookie(env);
  const first = await native(env, "/ws", cookie);
  const helloOld = await viewerHello(env, first.socket);
  // An upgrade made under the old epoch, before the replacement.
  const early = await native(env, `/media-ws?epoch=${helloOld.mediaEpoch}`, cookie);
  // A replacement signaling socket on the same PC takes a new epoch and retires the old epoch's auxiliary sockets.
  const second = await native(env, "/ws", cookie);
  const helloNew = await viewerHello(env, second.socket);
  assert.notEqual(helloNew.mediaEpoch, helloOld.mediaEpoch);
  assert.ok(early.socket.closed.length > 0, "the upgrade made under the old epoch is retired with it");

  // An upgrade that still carries the old epoch lands after the replacement: its hello is refused by the binding.
  const late = await native(env, `/media-ws?epoch=${helloOld.mediaEpoch}`, cookie);
  assert.equal(late.status, 101);
  await send(env, late.socket, { type: "hello", role: "media-client" });
  assertSocketRefused(late.socket, "old epoch after the replacement");
  assert.equal(await validate(env, PC.id, { role: "viewer", epoch: helloOld.mediaEpoch, defaultPc: true, requireTicket: false }), 404);
  assert.equal(await validate(env, PC.id, { role: "viewer", epoch: helloNew.mediaEpoch, defaultPc: true, requireTicket: false }), 204);

  const fresh = await native(env, `/media-ws?epoch=${helloNew.mediaEpoch}`, cookie);
  await send(env, fresh.socket, { type: "hello", role: "media-client" });
  assert.equal(framesOfType(fresh.socket, "hello-ok").length, 1, "the new epoch is admitted");
});

test("a caller's X-Relay-Default-Pc header cannot skip the pc-less epoch check, and cannot mark an explicit pc", async () => {
  const env = relayEnv();
  await onlineHost(env, PC);
  const cookie = await browserCookie(env);
  const viewer = await native(env, "/ws", cookie);
  const hello = await viewerHello(env, viewer.socket);

  // Pc-less, stale epoch, caller says "0": the Worker sets its own marker, so the stale epoch is refused.
  const spoofed = await connect(env, `/media-ws?epoch=${E_B}`, { headers: { Cookie: cookie, "X-Relay-Default-Pc": "0" }, origin: null });
  assert.equal(spoofed.socket.attachment.defaultPc, true, "marker set by the Worker");
  await send(env, spoofed.socket, { type: "hello", role: "media-client" });
  assertSocketRefused(spoofed.socket, "spoofed header, stale epoch");

  // Explicit pc, caller says "1": the header is stripped, so the socket is not marked and admission is unchanged.
  const explicit = await connect(env, `/media-ws?pc=${PC.id}&epoch=${E_B}`, { headers: { Cookie: cookie, "X-Relay-Default-Pc": "1" }, origin: null });
  assert.equal(explicit.socket.attachment.defaultPc, false);
  await send(env, explicit.socket, { type: "hello", role: "media-client" });
  assert.equal(framesOfType(explicit.socket, "hello-ok").length, 1, "explicit pc, no ticket: unchanged");

  // The real epoch, pc-less, with the header set by the caller: still admitted.
  const real = await connect(env, `/media-ws?epoch=${hello.mediaEpoch}`, { headers: { Cookie: cookie, "X-Relay-Default-Pc": "1" }, origin: null });
  await send(env, real.socket, { type: "hello", role: "media-client" });
  assert.equal(framesOfType(real.socket, "hello-ok").length, 1, "real epoch, pc-less");
});

test("pc-less lab media and file are bound to their own run's epoch, never a production PC, and a spoofed header does not bypass it", async () => {
  const env = relayEnv();
  await onlineHost(env, PC); // A production PC is online; the lab must not use it.
  const cookie = await browserCookie(env);
  const RUN_B = "123e4567-e89b-42d3-a456-426614174001";
  const labQuery = `run=${RUN}&compat=${COMPAT}`;

  const lab = await native(env, `/lab-ws?${labQuery}`, cookie);
  assert.equal(lab.socket.attachment.roomName, `lab-signal-v1:${RUN}`);
  const hello = await viewerHello(env, lab.socket);
  assert.equal(hello.pc, undefined, "lab signaling names no PC");

  // Valid: media and file with the run's own epoch, no pc and no ticket.
  const media = await native(env, `/lab-media-ws?${labQuery}&epoch=${hello.mediaEpoch}`, cookie);
  assert.equal(media.socket.attachment.pc, null, "no production PC");
  await send(env, media.socket, { type: "hello", role: "media-client" });
  assert.equal(framesOfType(media.socket, "hello-ok").length, 1, "valid lab media");
  assert.equal(lastValidate(env).name, `lab-signal-v1:${RUN}`, "validated in the run's signaling room");
  const file = await native(env, `/lab-file-ws?${labQuery}&epoch=${hello.mediaEpoch}`, cookie);
  await send(env, file.socket, { type: "hello", role: "file-client" });
  assert.equal(framesOfType(file.socket, "hello-ok").length, 1, "valid lab file");

  // Wrong run: the epoch belongs to RUN, so RUN_B's room has no such viewer.
  const wrongRun = await native(env, `/lab-media-ws?run=${RUN_B}&compat=${COMPAT}&epoch=${hello.mediaEpoch}`, cookie);
  await send(env, wrongRun.socket, { type: "hello", role: "media-client" });
  assertSocketRefused(wrongRun.socket, "epoch from another run");
  assert.equal(lastValidate(env).name, `lab-signal-v1:${RUN_B}`);

  // Stale epoch on the right run.
  const stale = await native(env, `/lab-file-ws?${labQuery}&epoch=${E_B}`, cookie);
  await send(env, stale.socket, { type: "hello", role: "file-client" });
  assertSocketRefused(stale.socket, "stale epoch on the right run");

  // Spoofed header on a pc-less lab socket with a stale epoch: still refused.
  const spoofed = await connect(env, `/lab-media-ws?${labQuery}&epoch=${E_B}`, { headers: { Cookie: cookie, "X-Relay-Default-Pc": "0" }, origin: null });
  await send(env, spoofed.socket, { type: "hello", role: "media-client" });
  assertSocketRefused(spoofed.socket, "spoofed header on a lab twin");
});

test("a host ticket stops admitting host media once its issuing host signaling socket closes", async () => {
  const env = relayEnv();
  const host = await onlineHost(env, PC);
  const before = await connect(env, `/host-media-ws?pc=${PC.id}&epoch=${E_A}`, { origin: null });
  await send(env, before.socket, { type: "hello", role: "media-host", ticket: host.reply.ticket });
  assert.equal(framesOfType(before.socket, "hello-ok").length, 1, "ticket admits while the host is online");

  await close(env, host.socket, 1000);
  const after = await connect(env, `/host-media-ws?pc=${PC.id}&epoch=${E_B}`, { origin: null });
  await send(env, after.socket, { type: "hello", role: "media-host", ticket: host.reply.ticket });
  assertSocketRefused(after.socket, "host ticket after its host socket closed");
});

test("a host ticket is refused for another PC and as a viewer; a viewer ticket is refused as a host", async () => {
  const env = relayEnv();
  const hostA = await onlineHost(env, PC);
  await onlineHost(env, ALIAS_PC);
  const cookie = await browserCookie(env);
  const viewer = await viewerConnect(env, `/ws?pc=${PC.id}`, cookie);
  const hello = await viewerHello(env, viewer.socket);

  const hostTicketOtherPc = await connect(env, `/host-media-ws?pc=${ALIAS_PC.id}&epoch=${E_A}`, { origin: null });
  await send(env, hostTicketOtherPc.socket, { type: "hello", role: "media-host", ticket: hostA.reply.ticket });
  assertSocketRefused(hostTicketOtherPc.socket, "host ticket of PC A on PC B");

  const hostTicketAsViewer = await viewerConnect(env, `/media-ws?pc=${PC.id}&epoch=${hello.mediaEpoch}`, cookie);
  await send(env, hostTicketAsViewer.socket, { type: "hello", role: "media-client", ticket: hostA.reply.ticket });
  assertSocketRefused(hostTicketAsViewer.socket, "host ticket presented as a viewer");

  const viewerTicketAsHost = await connect(env, `/host-media-ws?pc=${PC.id}&epoch=${E_A}`, { origin: null });
  await send(env, viewerTicketAsHost.socket, { type: "hello", role: "media-host", ticket: hello.ticket });
  assertSocketRefused(viewerTicketAsHost.socket, "viewer ticket presented as a host");
});

test("host media and host file upgrade and open without a bearer when they present a valid host ticket", async () => {
  const env = relayEnv();
  const host = await onlineHost(env, PC);
  for (const [path, role] of [
    [`/host-media-ws?pc=${PC.id}&epoch=${E_A}`, "media-host"],
    [`/host-file-ws?pc=${PC.id}&epoch=${E_A}`, "file-host"],
  ]) {
    const socket = await connect(env, path, { origin: null });
    assert.equal(socket.status, 101, `${role} upgrades without a bearer`);
    await send(env, socket.socket, { type: "hello", role, ticket: host.reply.ticket });
    assert.equal(framesOfType(socket.socket, "hello-ok").length, 1, `${role} opens with the ticket alone`);
  }
});

test("a replacement viewer retires only its PC's old epoch; the other PC's media stays open", async () => {
  const env = relayEnv();
  await onlineHost(env, PC);
  await onlineHost(env, ALIAS_PC);
  const cookie = await browserCookie(env);
  const oldViewerA = await viewerConnect(env, `/ws?pc=${PC.id}`, cookie);
  const oldHelloA = await viewerHello(env, oldViewerA.socket);
  const mediaA = await viewerConnect(env, `/media-ws?pc=${PC.id}&epoch=${oldHelloA.mediaEpoch}`, cookie);
  await send(env, mediaA.socket, { type: "hello", role: "media-client", ticket: oldHelloA.ticket });
  const viewerB = await viewerConnect(env, `/ws?pc=${ALIAS_PC.id}`, cookie);
  const helloB = await viewerHello(env, viewerB.socket);
  const mediaB = await viewerConnect(env, `/media-ws?pc=${ALIAS_PC.id}&epoch=${helloB.mediaEpoch}`, cookie);
  await send(env, mediaB.socket, { type: "hello", role: "media-client", ticket: helloB.ticket });

  const newViewerA = await viewerConnect(env, `/ws?pc=${PC.id}`, cookie);
  const newHelloA = await viewerHello(env, newViewerA.socket);
  assert.notEqual(newHelloA.mediaEpoch, oldHelloA.mediaEpoch);
  assert.ok(oldViewerA.socket.closed.length > 0, "the replaced viewer signaling socket is closed");
  assert.ok(mediaA.socket.closed.length > 0, "the old epoch's media socket is retired");
  assert.equal(await validate(env, PC.id, { role: "viewer", epoch: oldHelloA.mediaEpoch, ticket: oldHelloA.ticket, requireTicket: false }), 404, "old ticket is dead");
  assert.equal(await validate(env, PC.id, { role: "viewer", epoch: newHelloA.mediaEpoch, ticket: newHelloA.ticket, requireTicket: false }), 204, "new ticket is live");
  assert.equal(mediaB.socket.closed.length, 0, "PC B's media stays open");
  assert.equal(viewerB.socket.closed.length, 0, "PC B's viewer stays connected");
  assert.equal(await validate(env, ALIAS_PC.id, { role: "viewer", epoch: helloB.mediaEpoch, ticket: helloB.ticket, requireTicket: false }), 204, "PC B's ticket is live");
});

test("a Room that wakes from hibernation keeps tickets, online state and an issued registration challenge", async () => {
  const env = relayEnv();
  const host = await onlineHost(env, PC);
  const cookie = await browserCookie(env);
  const viewer = await viewerConnect(env, `/ws?pc=${PC.id}`, cookie);
  const hello = await viewerHello(env, viewer.socket);
  const pending = await registerHost(env, { identity: ALIAS_PC, token: env.TOKEN, sign: false });
  assert.equal(pending.challenge?.type, "register-challenge", "challenge issued before the wake");

  env.ROOM.wake(`pc-v1:${PC.id}`);
  env.ROOM.wake(`pc-v1:${ALIAS_PC.id}`);

  assert.equal(await validate(env, PC.id, { role: "viewer", epoch: hello.mediaEpoch, ticket: hello.ticket, requireTicket: false }), 204, "viewer ticket after wake");
  const media = await viewerConnect(env, `/media-ws?pc=${PC.id}&epoch=${hello.mediaEpoch}`, cookie);
  await send(env, media.socket, { type: "hello", role: "media-client", ticket: hello.ticket });
  assert.equal(framesOfType(media.socket, "hello-ok").length, 1, "viewer media opens after wake");

  const hostMedia = await connect(env, `/host-media-ws?pc=${PC.id}&epoch=${E_A}`, { origin: null });
  await send(env, hostMedia.socket, { type: "hello", role: "media-host", ticket: host.reply.ticket });
  assert.equal(framesOfType(hostMedia.socket, "hello-ok").length, 1, "host ticket after wake");

  assert.deepEqual(await (await env.ROOM.instance(`pc-v1:${PC.id}`).fetch(new Request("https://room.internal/__pc/online"))).json(), { online: true }, "online after wake");

  const signature = await signRelayProof({ seedHex: ALIAS_PC.seedHex, role: "host", origin: RELAY_ORIGIN, nonce: pending.challenge.nonce, id: ALIAS_PC.id, public: ALIAS_PC.public });
  await send(env, pending.socket, { type: "register-proof", signature });
  assert.equal(framesOfType(pending.socket, "hello-ok")[0]?.pc, ALIAS_PC.id, "challenge completes after wake");
});

test("admin deletion after a wake still closes host media and file sockets opened before any viewer", async () => {
  const env = relayEnv();
  const host = await onlineHost(env, PC);
  const bearer = { Authorization: `Bearer ${env.HOST_GATE_TOKEN}` };
  const hostMedia = await connect(env, `/host-media-ws?pc=${PC.id}&epoch=${E_A}`, { headers: bearer, origin: null });
  await send(env, hostMedia.socket, { type: "hello", role: "media-host" });
  const hostFile = await connect(env, `/host-file-ws?pc=${PC.id}&epoch=${E_A}`, { headers: bearer, origin: null });
  await send(env, hostFile.socket, { type: "hello", role: "file-host" });

  env.ROOM.wake(`pc-v1:${PC.id}`);
  const deleted = await env.GATE.get().fetch(new Request(`${RELAY_ORIGIN}/__gate/manage/pcs/${PC.id}`, {
    method: "DELETE", headers: { Authorization: `Bearer ${env.GATE_ADMIN_TOKEN}`, Origin: RELAY_ORIGIN },
  }));
  assert.equal(deleted.status, 204);
  assert.ok(host.socket.closed.length > 0, "signaling host closed after wake");
  assert.ok(hostMedia.socket.closed.length > 0, "host media closed after wake");
  assert.ok(hostFile.socket.closed.length > 0, "host file closed after wake");
});
