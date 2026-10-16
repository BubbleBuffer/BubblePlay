// Integrated fake Durable Object namespaces for the phase-1 routing tests. The
// Worker's own fetch(), the real ApprovalGate (GATE) and the real Room and
// MediaRoom classes run unchanged; only the platform is faked: WebSocketPair,
// hibernation-style state, storage and namespace routing. Each DO name gets its
// own state, so two PCs or a PC and the legacy room never share sockets or storage.
import { createHash } from "node:crypto";
import worker, { Room, MediaRoom } from "../../src/index.js";
import { ApprovalGate } from "../../src/gate.js";
import { RELAY_SECRETS } from "./relay-secrets.mjs";
import { PC, RELAY_ORIGIN, signRelayProof } from "./dk-contract.mjs";

export { worker, RELAY_SECRETS };

// Workers answer a WebSocket upgrade with status 101 and a webSocket property. Node's
// Response rejects 101, so the shim reports 200 and keeps the socket on the object.
const NativeResponse = globalThis.Response;
globalThis.Response = class extends NativeResponse {
  constructor(body, init) {
    super(body, init?.status === 101 ? { ...init, status: 200 } : init);
    if (init?.webSocket) this.webSocket = init.webSocket;
  }
};

export class FakeSocket {
  constructor(role, extra = {}) { this.attachment = { role, ...extra }; this.readyState = 1; this.messages = []; this.closed = []; this.peer = null; }
  deserializeAttachment() { return structuredClone(this.attachment); }
  serializeAttachment(value) { this.attachment = structuredClone(value); }
  // Frames the Room wrote to this socket's client.
  send(raw) {
    if (this.readyState !== 1) throw new Error("send on closed socket");
    this.messages.push(raw);
  }
  close(code, reason) { this.closed.push({ code, reason }); this.readyState = 3; }
}

globalThis.WebSocketPair = class {
  constructor() {
    const client = new FakeSocket(null);
    const server = new FakeSocket(null);
    client.peer = server;
    server.peer = client;
    this[0] = client;
    this[1] = server;
  }
};

class FakeStorage {
  constructor() { this.values = new Map(); this.alarm = null; }
  async get(key) { return structuredClone(this.values.get(key)); }
  async put(key, value) { this.values.set(key, structuredClone(value)); }
  async delete(key) { return this.values.delete(key); }
  async list({ prefix = "" } = {}) {
    return new Map([...this.values].filter(([key]) => key.startsWith(prefix)).map(([key, value]) => [key, structuredClone(value)]));
  }
  async getAlarm() { return this.alarm; }
  async setAlarm(at) { this.alarm = at; }
}

export class FakeState {
  constructor() { this.sockets = []; this.storage = new FakeStorage(); }
  acceptWebSocket(ws) { this.sockets.push(ws); }
  getWebSockets() { return this.sockets.filter((ws) => ws.readyState === 1); }
  blockConcurrencyWhile(operation) { return operation(); }
}

// One namespace per binding. get(name) routes fetch() to the instance for that
// name, creating it on first use, and logs every call with its body.
export class FakeNamespace {
  constructor(binding, env, ClassRef, log) {
    this.binding = binding; this.env = env; this.ClassRef = ClassRef; this.log = log; this.instances = new Map();
  }
  idFromName(name) { return name; }
  instance(name) {
    if (!this.instances.has(name)) {
      const state = new FakeState();
      this.instances.set(name, { state, room: new this.ClassRef(state, this.env) });
    }
    return this.instances.get(name).room;
  }
  state(name) { this.instance(name); return this.instances.get(name).state; }
  // Hibernation wake: a fresh Room on the same state, sockets and storage, as the platform
  // builds one when a hibernated object receives its next event.
  wake(name) {
    const entry = this.instances.get(name);
    if (!entry) throw new Error(`no instance to wake: ${name}`);
    entry.room = new this.ClassRef(entry.state, this.env);
    return entry.room;
  }
  get(name) {
    const namespace = this;
    return {
      fetch: async (request) => {
        const url = new URL(request.url);
        const text = request.method === "POST" ? await request.clone().text() : "";
        let body = null;
        try { body = text ? JSON.parse(text) : null; } catch { body = text; }
        namespace.log.push({ binding: namespace.binding, name, path: url.pathname, method: request.method, body });
        return namespace.instance(name).fetch(request);
      },
    };
  }
}

// Environment with a real ApprovalGate and real Room/MediaRoom namespaces. The
// test log records every namespace call, including the gate's own traffic.
export function relayEnv() {
  const log = [];
  const env = {
    ...RELAY_SECRETS,
    APPROVAL_METHODS: "ntfy",
    NTFY_URL: "https://ntfy.example.test",
    NTFY_TOPIC: "test",
    NTFY_TOKEN: "test-token",
    ASSETS: { fetch: async () => new Response("Sign in") },
  };
  const gateState = new FakeState();
  const gate = new ApprovalGate(gateState, env);
  env.GATE = { idFromName: () => "global-v1", get: () => gate };
  env.ROOM = new FakeNamespace("ROOM", env, Room, log);
  env.MEDIA_ROOM = new FakeNamespace("MEDIA_ROOM", env, MediaRoom, log);
  // Not part of the Worker's contract: test-only handles for the helpers below.
  Object.defineProperty(env, "testLog", { value: log });
  Object.defineProperty(env, "gateState", { value: gateState });
  return env;
}

const BROWSER_GATE_TOKEN = "synthetic-approved-browser";
// Both cookies a viewer needs: the site gate and the session login (phase 1 keeps it).
export async function browserCookie(env) {
  const gateHash = createHash("sha256").update(BROWSER_GATE_TOKEN).digest("hex");
  await env.gateState.storage.put(`session:${gateHash}`, {
    id: "dk-browser", createdAt: Date.now(), lastSeenAt: Date.now(), expiresAt: Date.now() + 24 * 3600_000,
  });
  const login = await env.GATE.get().fetch(new Request(`${RELAY_ORIGIN}/__auth/login`, {
    method: "POST",
    headers: { Cookie: `__Host-gate=${BROWSER_GATE_TOKEN}`, Origin: RELAY_ORIGIN, "Content-Type": "application/json" },
    body: JSON.stringify({ password: env.APP_PASSWORD }),
  }));
  if (login.status !== 204) throw new Error(`test login failed: ${login.status}`);
  const session = login.headers.get("Set-Cookie").match(/__Host-bp-session=[^;,]+/)[0];
  return `__Host-gate=${BROWSER_GATE_TOKEN}; ${session}`;
}

// Upgrades through the real Worker fetch. Returns the Worker's status (101 for a
// socket), the server side the Room holds, and the client side the test drives.
export async function connect(env, path, { headers = {}, origin = RELAY_ORIGIN } = {}) {
  const requestHeaders = { Upgrade: "websocket", ...headers };
  if (origin !== null) requestHeaders.Origin = origin;
  const response = await worker.fetch(new Request(`${origin ?? RELAY_ORIGIN}${path}`, { headers: requestHeaders }), env);
  if (!response.webSocket) return { status: response.status, socket: null, client: null };
  return { status: 101, socket: response.webSocket.peer, client: response.webSocket };
}

// Delivers one frame from a peer to the Room that owns its socket.
export async function send(env, socket, message) {
  const raw = typeof message === "string" ? message : JSON.stringify(message);
  const { roomName, namespace } = socket.attachment;
  return env[namespace].instance(roomName).webSocketMessage(socket, raw);
}

// Text frames the Room wrote to this socket's peer, parsed.
export function frames(socket) {
  return socket.messages.filter((raw) => typeof raw === "string").map((raw) => JSON.parse(raw));
}
export const framesOfType = (socket, type) => frames(socket).filter((frame) => frame.type === type);

// Socket closes: the peer side goes to state 3 and the Room gets the close callback.
export async function close(env, socket, code = 1000, reason = "") {
  socket.readyState = 3;
  const { roomName, namespace } = socket.attachment;
  return env[namespace].instance(roomName).webSocketClose(socket, code, reason, true);
}

// Host registration on /host-ws (or /lab-host-ws, /host-media-ws, ...). Sends the
// hello, reads register-challenge, and answers with a proof signed by the seed.
// Options let a test send a token, a wrong ID or key, a stale nonce, a fixture
// signature, or no proof at all. Returns whatever the relay did; a test asserts on it.
export async function registerHost(env, {
  identity = PC, path = "/host-ws", pc = identity.id, params = [], bearer = true, origin = RELAY_ORIGIN,
  helloId = identity.id, helloPublic = identity.public, token, role = "host",
  noId = false, sign = true, nonce, signature,
} = {}) {
  const query = [...(pc === null ? [] : [`pc=${encodeURIComponent(pc)}`]), ...params];
  const headers = bearer ? { Authorization: `Bearer ${env.HOST_GATE_TOKEN}` } : {};
  const { status, socket } = await connect(env, `${path}${query.length ? `?${query.join("&")}` : ""}`, { headers, origin: null });
  if (!socket) return { status, socket: null, challenge: null, reply: null, frames: [] };
  const hello = { type: "hello", role, ...(noId ? {} : { id: helloId, public: helloPublic }),
    ...(token === undefined ? {} : { token }) };
  await send(env, socket, hello);
  const challenge = framesOfType(socket, "register-challenge")[0] ?? null;
  if (challenge && !noId && (sign || signature !== undefined)) {
    // Signed over the identity's canonical ID and key, whatever spelling the hello used.
    const proof = signature ?? await signRelayProof({
      seedHex: identity.seedHex, role: "host", origin, nonce: nonce ?? challenge.nonce, id: identity.id, public: identity.public,
    });
    await send(env, socket, { type: "register-proof", signature: proof });
  }
  return { status, socket, challenge, reply: framesOfType(socket, "hello-ok")[0] ?? null, frames: frames(socket) };
}

// Viewer on /ws, /media-ws or /file-ws: cookies and same-origin Origin, as a browser sends.
export async function viewerConnect(env, path, cookie, extraHeaders = {}) {
  return connect(env, path, { headers: { Cookie: cookie, ...extraHeaders } });
}

// Viewer signaling hello. Returns the hello-ok, or null when the Room refused it.
export async function viewerHello(env, socket, extra = {}) {
  await send(env, socket, { type: "hello", role: "client", ...extra });
  return framesOfType(socket, "hello-ok")[0] ?? null;
}
