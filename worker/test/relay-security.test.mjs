import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import worker, { Room } from "../src/index.js";
import { validApprovalMessage } from "../src/protocol-validation.js";
import { RELAY_SECRETS } from "./helpers/relay-secrets.mjs";
import { ApprovalGate } from "../src/gate.js";

const contract = JSON.parse(readFileSync(new URL("../../fixtures/browser-approval-contract.json", import.meta.url)));
const identity = { gateHash: "a".repeat(64), authHash: "b".repeat(64), expiresAt: Date.now() + 60_000 };

class Socket {
  constructor(role, extra = {}) { this.attachment = { role, ...extra }; this.readyState = 1; this.messages = []; this.closed = []; }
  deserializeAttachment() { return structuredClone(this.attachment); }
  serializeAttachment(value) { this.attachment = structuredClone(value); }
  send(raw) { if (this.failSend) throw new Error("synthetic send failure"); this.messages.push(raw); }
  close(code, reason) { this.closed.push({ code, reason }); this.readyState = 3; }
}

function fixture(sockets = [new Socket("host")]) {
  const values = new Map();
  const calls = [];
  let valid = true;
  let scheduled = null;
  const state = {
    getWebSockets: () => sockets,
    storage: {
      get: async key => structuredClone(values.get(key)),
      put: async (key, value) => values.set(key, structuredClone(value)),
      delete: async key => values.delete(key),
      getAlarm: async () => scheduled,
      setAlarm: async at => { scheduled = at; },
    },
  };
  const env = { TOKEN: RELAY_SECRETS.TOKEN, GATE: {
    idFromName: name => name,
    get: () => ({ fetch: async request => {
      calls.push({ path: new URL(request.url).pathname, body: request.method === "POST" ? await request.json() : null });
      return new Response(null, { status: valid ? 204 : 404 });
    } }),
  } };
  return { room: new Room(state, env), state, env, sockets, calls, values,
    revoke: () => { valid = false; }, scheduled: () => scheduled };
}

function internal(path, message) {
  return new Request(`https://room.internal${path}`, message === undefined ? {} : { method: "POST", body: JSON.stringify(message) });
}

test("Worker approval messages validate the shared fixture and its invalid examples", () => {
  for (const message of [contract.capability, contract.request, contract.decision, contract.cancel]) assert.equal(validApprovalMessage(message), true);
  for (const message of contract.invalid) assert.equal(validApprovalMessage(message), false);
  for (const message of [{ ...contract.decision, injected: true }, { ...contract.request, id: "x".repeat(65) },
    { ...contract.request, agent: "a".repeat(81) }, { ...contract.request, expiresAt: 1.5 }]) assert.equal(validApprovalMessage(message), false);
});

test("only a capable host receives requests; decisions are one-shot and socket-owned", async () => {
  const host = new Socket("host");
  const client = new Socket("client", { sessionIdentity: identity });
  const f = fixture([host, client]);
  const request = { ...contract.request, expiresAt: Date.now() + 30_000 };
  assert.deepEqual(await (await f.room.fetch(internal("/__approval/status"))).json(), { hostOnline: false });
  assert.equal((await f.room.fetch(internal("/__approval/request", request))).status, 503);
  await f.room.webSocketMessage(host, JSON.stringify(contract.capability));
  assert.deepEqual(await (await f.room.fetch(internal("/__approval/status"))).json(), { hostOnline: true });
  assert.equal((await f.room.fetch(internal("/__approval/request", request))).status, 204);
  assert.deepEqual(JSON.parse(host.messages.at(-1)), request);
  assert.ok(f.scheduled() <= Date.now() + 5_000);
  await f.room.webSocketMessage(client, JSON.stringify(contract.decision));
  const stranger = new Socket("host");
  await f.room.webSocketMessage(stranger, JSON.stringify(contract.decision));
  assert.equal(f.calls.filter(call => call.path === "/__approval/decision").length, 0);
  // The attachment, rather than an in-memory request map, survives hibernation.
  f.room = new Room(f.state, f.env);
  await f.room.webSocketMessage(host, JSON.stringify(contract.decision));
  await f.room.webSocketMessage(host, JSON.stringify(contract.decision));
  assert.deepEqual(f.calls.filter(call => call.path === "/__approval/decision").map(call => call.body), [contract.decision]);
});

test("host cancellation and expiry remove authority before late decisions", async () => {
  const host = new Socket("host", { browserApproval: true });
  const f = fixture([host]);
  const request = { ...contract.request, expiresAt: Date.now() + 30_000 };
  await f.room.fetch(internal("/__approval/request", request));
  await f.room.fetch(internal("/__approval/cancel", contract.cancel));
  await f.room.webSocketMessage(host, JSON.stringify(contract.decision));
  assert.deepEqual(JSON.parse(host.messages.at(-1)), contract.cancel);
  await f.room.fetch(internal("/__approval/request", request));
  host.attachment.approvalRequests[request.id] = Date.now() - 1;
  await f.room.webSocketMessage(host, JSON.stringify(contract.decision));
  await f.room.alarm();
  assert.deepEqual(JSON.parse(host.messages.at(-1)), contract.cancel);
  assert.equal(f.calls.length, 0);
});

test("approval internal routes cannot be reached through the public Worker", async () => {
  for (const path of ["/__approval/request", "/__approval/cancel", "/__approval/decision", "/__approval/status",
    "/__auth/socket", "/__auth/socket-register", "/__auth/socket-unregister", "/__auth/recheck"]) {
    assert.equal((await worker.fetch(new Request(`https://play.example${path}`), {})).status, 404);
  }
});

test("all browser channels check sessions before incoming frames and close revoked sockets", async () => {
  for (const role of ["client", "media-client", "file-client"]) {
    const client = new Socket(role, { sessionIdentity: identity });
    const f = fixture([client]);
    f.revoke();
    await f.room.webSocketMessage(client, JSON.stringify({ type: "ping" }));
    assert.deepEqual(client.messages, []);
    assert.deepEqual(client.closed, [{ code: 1008, reason: "browser session revoked" }]);
  }
});

test("idle alarms and explicit rechecks bypass session cache, including after hibernation", async () => {
  for (const recheck of [room => room.alarm(), room => room.fetch(internal("/__auth/recheck", {}))]) {
    const sockets = ["client", "media-client", "file-client"].map(role => new Socket(role, { sessionIdentity: identity }));
    const f = fixture(sockets);
    await f.room.webSocketMessage(sockets[0], JSON.stringify({ type: "ping" }));
    await f.room.webSocketMessage(sockets[0], JSON.stringify({ type: "ping" }));
    assert.equal(f.calls.length, 1);
    f.revoke();
    await recheck(f.room);
    for (const socket of sockets) assert.equal(socket.closed[0].code, 1008);
    assert.equal(f.calls.length, 4);
  }
  const client = new Socket("client", { sessionIdentity: identity });
  const f = fixture([client]);
  await f.room.webSocketMessage(client, JSON.stringify({ type: "ping" }));
  f.revoke();
  await new Room(f.state, f.env).alarm();
  assert.equal(client.closed[0].code, 1008);
});

test("legacy sockets without session identity fail closed and live idle alarms recur within5s", async () => {
  const client = new Socket("client");
  const f = fixture([client]);
  await f.room.webSocketMessage(client, JSON.stringify({ type: "ping" }));
  assert.equal(client.closed[0].code, 1008);
  const live = fixture([new Socket("client", { sessionIdentity: identity })]);
  await live.room.alarm();
  assert.ok(live.scheduled() >= Date.now() - 10 && live.scheduled() <= Date.now() + 5_000);
});

test("Room reconstruction validates legacy attachments before host traffic and restores idle checks", async () => {
  const client = new Socket("client");
  const f = fixture([client]);
  let initialized;
  f.state.blockConcurrencyWhile = operation => { initialized = operation(); };
  new Room(f.state, f.env);
  await initialized;
  assert.equal(client.closed[0].code, 1008);
  assert.ok(f.scheduled() <= Date.now() + 5_000);
});

test("signaling frames have the Go512KiB byte limit before JSON parsing", async () => {
  for (const raw of ["{" + "x".repeat(512 * 1024), JSON.stringify({ type: "ice", candidate: "é".repeat(256 * 1024) })]) {
    const host = new Socket("host");
    const f = fixture([host]);
    await f.room.webSocketMessage(host, raw);
    assert.deepEqual(host.closed, [{ code: 1009, reason: "signaling message too large" }]);
  }
});

test("pending offers and ICE have a total byte budget and close overflowing sender", async () => {
  const client = new Socket("client", { sessionIdentity: identity });
  const f = fixture([client]);
  const raw = JSON.stringify({ type: "offer", sdp: "a".repeat(300 * 1024) });
  await f.room.webSocketMessage(client, raw);
  assert.equal(f.values.get("pendingToHost").length, 1);
  await f.room.webSocketMessage(client, JSON.stringify({ type: "ice", candidate: "b".repeat(300 * 1024) }));
  assert.deepEqual(client.closed, [{ code: 1013, reason: "pending queue full" }]);
  assert.equal(f.values.has("pendingToHost"), false);
});

test("a failed peer send closes that peer with a clear retry code", async () => {
  const host = new Socket("host");
  const client = new Socket("client", { sessionIdentity: identity });
  const f = fixture([host, client]);
  host.failSend = true;
  await f.room.webSocketMessage(client, JSON.stringify({ type: "offer", sdp: "bounded" }));
  assert.deepEqual(host.closed, [{ code: 1013, reason: "peer send failed" }]);
});

async function authenticatedRooms() {
  const values = new Map();
  const rooms = new Map();
  const background = [];
  const env = { ...RELAY_SECRETS, APPROVAL_METHODS: "ntfy",
    NTFY_URL: "https://ntfy.example.test", NTFY_TOPIC: "test", NTFY_TOKEN: "test-token",
    ASSETS: { fetch: async () => new Response("Sign in") },
  };
  const storage = {
    get: async key => structuredClone(values.get(key)),
    put: async (key, value) => values.set(key, structuredClone(value)),
    delete: async key => values.delete(key),
    list: async ({ prefix }) => new Map([...values].filter(([key]) => key.startsWith(prefix)).map(([key, value]) => [key, structuredClone(value)])),
  };
  const gate = new ApprovalGate({ storage, waitUntil: promise => background.push(promise) }, env);
  env.GATE = { idFromName: name => name, get: () => gate };
  for (const namespace of ["ROOM", "MEDIA_ROOM"]) env[namespace] = {
    idFromName: name => name, get: name => rooms.get(`${namespace}:${name}`).room,
  };
  const gateToken = "synthetic-approved-browser";
  const gateHash = Buffer.from(await crypto.subtle.digest("SHA-256", new TextEncoder().encode(gateToken))).toString("hex");
  await storage.put(`session:${gateHash}`, { id: "browser-to-revoke", createdAt: Date.now(), lastSeenAt: Date.now(), expiresAt: Date.now() + 60_000 });
  const login = await gate.fetch(new Request("https://play.example/__auth/login", { method: "POST",
    headers: { Cookie: `__Host-gate=${gateToken}`, Origin: "https://play.example", "Content-Type": "application/json" },
    body: JSON.stringify({ password: env.APP_PASSWORD }) }));
  assert.equal(login.status, 204);
  const cookie = `__Host-gate=${gateToken}; ${login.headers.get("Set-Cookie").match(/__Host-bp-session=[^;,]+/)[0]}`;
  const check = await gate.fetch(new Request("https://gate.internal/__auth/socket", { headers: { Cookie: cookie } }));
  const bound = await check.json();
  const sockets = [];
  for (const [role, roomName, namespace] of [["client", "room-v3", "ROOM"], ["media-client", "media-v4:epoch", "MEDIA_ROOM"],
    ["file-client", "file-v1:epoch", "MEDIA_ROOM"]]) {
    const socket = new Socket(role, { sessionIdentity: bound, roomName, namespace });
    const f = fixture([socket]);
    f.room = new Room(f.state, env);
    rooms.set(`${namespace}:${roomName}`, f);
    sockets.push(socket);
    const registered = await gate.fetch(new Request("https://gate.internal/__auth/socket-register", { method: "POST",
      body: JSON.stringify({ identity: bound, roomName, namespace }) }));
    assert.equal(registered.status, 204);
    await f.room.webSocketMessage(socket, JSON.stringify({ type: "ping" }));
  }
  return { gate, env, values, rooms, sockets, cookie, bound, background };
}

test("logout, forget-browser and targeted/all admin revocation immediately close all actual bound Rooms", async () => {
  for (const [method, path] of [["POST", "/__auth/logout"], ["POST", "/__auth/forget-browser"],
    ["DELETE", "/__gate/manage/sessions/browser-to-revoke"], ["DELETE", "/__gate/manage/sessions"],
    ["DELETE", "/__gate/manage/auth-sessions"]]) {
    const f = await authenticatedRooms();
    const response = await f.gate.fetch(new Request(`https://play.example${path}`, { method,
      headers: { Cookie: f.cookie, Origin: "https://play.example", Authorization: `Bearer ${f.env.GATE_ADMIN_TOKEN}` } }));
    assert.equal(response.status, 204, path);
    for (const socket of f.sockets) assert.deepEqual(socket.closed, [{ code: 1008, reason: "browser session revoked" }], path);
  }
});

test("session expiry and password rotation close idle signaling, media and file sockets", async () => {
  for (const cause of ["gate-expired", "auth-expired", "password-rotated"]) {
    const f = await authenticatedRooms();
    if (cause === "password-rotated") f.env.APP_PASSWORD = "new synthetic rotated password";
    else {
      const key = cause === "gate-expired" ? `session:${f.bound.gateHash}` : `auth-session:${f.bound.authHash}`;
      f.values.get(key).expiresAt = Date.now() - 1;
    }
    await Promise.all([...f.rooms.values()].map(room => room.room.alarm()));
    await Promise.all(f.background);
    for (const socket of f.sockets) assert.equal(socket.closed[0]?.code, 1008, cause);
  }
});

test("browser admission stores session hashes on each signaling, media and file attachment", async () => {
  const f = await authenticatedRooms();
  const NativeResponse = globalThis.Response;
  const NativePair = globalThis.WebSocketPair;
  const accepted = [];
  globalThis.Response = class extends NativeResponse {
    constructor(body, init) {
      super(body, init?.status === 101 ? { ...init, status: 200 } : init);
      this.webSocket = init?.webSocket;
    }
  };
  globalThis.WebSocketPair = class {
    constructor() { this[0] = new Socket(null); this[1] = new Socket(null); }
  };
  try {
    for (const path of ["/ws", "/media-ws?epoch=" + "c".repeat(32), "/file-ws?epoch=" + "c".repeat(32)]) {
      const base = fixture([]);
      base.state.acceptWebSocket = socket => accepted.push(socket);
      const room = new Room(base.state, f.env);
      const response = await room.fetch(new Request(`https://play.example${path}`, { headers: { Cookie: f.cookie, Upgrade: "websocket" } }));
      assert.ok(response.webSocket);
      const att = accepted.at(-1).attachment;
      assert.equal(att.sessionIdentity.gateHash, f.bound.gateHash);
      assert.equal(att.sessionIdentity.authHash, f.bound.authHash);
      assert.equal(JSON.stringify(att).includes("synthetic-approved-browser"), false);
      assert.ok(base.scheduled() <= Date.now() + 5_000);
    }
  } finally { globalThis.Response = NativeResponse; globalThis.WebSocketPair = NativePair; }
});
