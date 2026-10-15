import test from "node:test";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { STREAM_LAB_COMPATIBILITY } from "../public/media.js";
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
