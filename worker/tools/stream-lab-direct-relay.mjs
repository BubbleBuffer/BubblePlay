import { randomBytes } from "node:crypto";
import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { extname, isAbsolute, join, normalize, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { WebSocket, WebSocketServer } from "ws";
import { STREAM_LAB_COMPATIBILITY } from "../public/media.js";
import {
  V4_HEADER_BYTES,
  V4_MAX_PACKET_BYTES,
  validateClientMessageV4,
  validateHostTextV4,
} from "../public/media-v4.js";

const UUID_PATTERN = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i;
const EPOCH_PATTERN = /^[a-f0-9]{32}$/;
const MAX_SIGNAL_BUFFER = 64;
const MAX_CLIENT_TRACE_BYTES = 16_384;
const PUBLIC_DIR = resolve(import.meta.dirname, "..", "public");
const MIME_TYPES = new Map([
  [".css", "text/css; charset=utf-8"],
  [".html", "text/html; charset=utf-8"],
  [".js", "text/javascript; charset=utf-8"],
  [".json", "application/json; charset=utf-8"],
  [".svg", "image/svg+xml"],
  [".woff2", "font/woff2"],
]);

export async function startDirectLabRelay(options = {}) {
  const host = options.host ?? "127.0.0.1";
  if (!["127.0.0.1", "::1", "localhost"].includes(host))
    throw new RangeError("direct Stream Lab relay must bind to loopback");
  const port = Number(options.port ?? 8787);
  if (!Number.isInteger(port) || port < 0 || port > 65_535) throw new RangeError("invalid relay port");
  const publicDir = resolve(options.publicDir ?? PUBLIC_DIR);
  const emit = typeof options.onEvent === "function"
    ? event => {
      try { options.onEvent({ at: new Date().toISOString(), ...event }); } catch { /* diagnostics must not disrupt relay */ }
    }
    : () => {};
  const shaper = createMediaShaper(options.throttle);
  const signalingRooms = new Map();
  const mediaRooms = new Map();
  const sockets = new Set();
  const webSockets = new WebSocketServer({
    noServer: true,
    maxPayload: V4_MAX_PACKET_BYTES,
  });
  const server = createServer((request, response) => serveRequest(request, response, publicDir, emit));
  server.on("upgrade", (request, socket, head) => {
    const route = parseSocketRoute(request.url);
    if (!route) {
      emit({ type: "socket-upgrade-rejected", path: socketPath(request.url) });
      socket.write("HTTP/1.1 404 Not Found\r\nConnection: close\r\n\r\n");
      socket.destroy();
      return;
    }
    emit({ type: "socket-upgrade", ...route });
    webSockets.handleUpgrade(request, socket, head, ws => {
      webSockets.emit("connection", ws, request, route);
    });
  });
  webSockets.on("connection", (socket, _request, route) => {
    sockets.add(socket);
    socket.once("close", () => sockets.delete(socket));
    if (route.kind === "signal") attachSignalingSocket(socket, route, signalingRooms, emit);
    else attachMediaSocket(socket, route, mediaRooms, emit, shaper);
  });
  await new Promise((resolveListen, reject) => {
    server.once("error", reject);
    server.listen(port, host, () => {
      server.removeListener("error", reject);
      resolveListen();
    });
  });
  const address = server.address();
  const actualHost = address.address === "::1" ? "[::1]" : address.address;
  return {
    url: `http://${actualHost}:${address.port}/`,
    /// Lab-only bottleneck on host->client media: `{ kbps, delayMs }`, or
    /// null for none. Applies to packets forwarded from now on.
    setThrottle: throttle => shaper.set(throttle),
    throttleStats: () => shaper.stats(),
    async close() {
      for (const socket of sockets) socket.terminate();
      await new Promise(resolveClose => webSockets.close(() => resolveClose()));
      await new Promise(resolveClose => server.close(() => resolveClose()));
    },
  };
}

function parseSocketRoute(rawUrl) {
  let url;
  try { url = new URL(rawUrl, "http://127.0.0.1"); } catch { return null; }
  const run = url.searchParams.get("run") || "";
  if (!UUID_PATTERN.test(run) ||
      url.searchParams.get("compat") !== STREAM_LAB_COMPATIBILITY.id) return null;
  if (url.pathname === "/lab-host-ws")
    return { kind: "signal", run, expectedRole: "host" };
  if (url.pathname === "/lab-ws")
    return { kind: "signal", run, expectedRole: "client" };
  const mediaHost = url.pathname === "/lab-host-media-ws";
  const mediaClient = url.pathname === "/lab-media-ws";
  if (!mediaHost && !mediaClient) return null;
  const epoch = url.searchParams.get("epoch") || "";
  if (!EPOCH_PATTERN.test(epoch)) return null;
  return {
    kind: "media",
    run,
    epoch,
    key: `${run}:${epoch}`,
    expectedRole: mediaHost ? "media-host" : "media-client",
  };
}

function attachSignalingSocket(socket, route, rooms, emit) {
  const room = getRoom(rooms, route.run, () => ({
    host: null,
    client: null,
    hostCapabilities: null,
    pendingToHost: [],
  }));
  let role = null;
  socket.on("message", (data, isBinary) => {
    if (!role) {
      const hello = isBinary ? null : parseJson(data);
      if (hello?.type !== "hello" || hello.role !== route.expectedRole) {
        socket.close(1008, "bad role");
        return;
      }
      role = route.expectedRole;
      replaceSocket(room, role, socket);
      emit({ type: "socket-hello", kind: route.kind, run: route.run, role });
      const response = role === "client"
        ? { type: "hello-ok", iceServers: [], mediaEpoch: randomBytes(16).toString("hex") }
        : { type: "hello-ok" };
      socket.send(JSON.stringify(response));
      if (role === "client" && room.hostCapabilities) socket.send(room.hostCapabilities);
      if (role === "host" && room.pendingToHost.length) {
        for (const message of room.pendingToHost.splice(0)) socket.send(message);
      }
      return;
    }
    if (!isBinary) {
      const message = parseJson(data);
      if (message?.type === "ping") {
        socket.send(JSON.stringify({ type: "pong" }));
        return;
      }
      if (role === "host" && message?.type === "host-capabilities")
        room.hostCapabilities = String(data);
    }
    const peer = role === "host" ? room.client : room.host;
    if (peer?.readyState === WebSocket.OPEN) {
      peer.send(data, { binary: isBinary });
    } else if (role === "client" && room.pendingToHost.length < MAX_SIGNAL_BUFFER) {
      room.pendingToHost.push(isBinary ? Buffer.from(data) : String(data));
    }
  });
  socket.once("close", (code, reason) => {
    emit({ type: "socket-close", kind: route.kind, run: route.run, role,
      code, reason: String(reason || "") });
    if (!role || room[role] !== socket) return;
    room[role] = null;
    const peer = role === "host" ? room.client : room.host;
    if (peer?.readyState === WebSocket.OPEN) peer.send(JSON.stringify({ type: "peer-left" }));
    if (!room.host && !room.client) rooms.delete(route.run);
  });
  socket.on("error", error => emit({ type: "socket-error", kind: route.kind, run: route.run,
    role, message: String(error?.message || error) }));
}

// Protocol v4: one socket per side. Host binary frames go to the client;
// client ACK/keyframe text goes to the host; heartbeats are answered here
// and each side is told whether its peer is present (mirrors worker/src).
function attachMediaSocket(socket, route, rooms, emit, shaper) {
  const room = getRoom(rooms, route.key, () => ({ "media-host": null, "media-client": null }));
  let role = null;
  const peerRole = () => (role === "media-host" ? "media-client" : "media-host");
  const notify = (target, present) => {
    if (target?.readyState === WebSocket.OPEN) target.send(JSON.stringify({ type: "media-peer", present }));
  };
  socket.on("message", (data, isBinary) => {
    if (!role) {
      const hello = isBinary ? null : parseJson(data);
      if (hello?.type !== "hello" || hello.role !== route.expectedRole) {
        socket.close(1008, "bad role");
        return;
      }
      role = route.expectedRole;
      replaceSocket(room, role, socket);
      emit({ type: "socket-hello", kind: route.kind, run: route.run, role });
      socket.send(JSON.stringify({ type: "hello-ok" }));
      const peer = room[peerRole()];
      notify(socket, peer?.readyState === WebSocket.OPEN);
      notify(peer, true);
      return;
    }
    const peer = room[peerRole()];
    if (isBinary) {
      if (role !== "media-host" || data.byteLength <= V4_HEADER_BYTES || data.byteLength > V4_MAX_PACKET_BYTES) return;
      if (peer?.readyState === WebSocket.OPEN) shaper.forward(peer, data);
      return;
    }
    const text = String(data);
    const message = role === "media-client" ? validateClientMessageV4(text) : validateHostTextV4(text);
    if (!message) return;
    if (message.type === "ping") {
      socket.send(JSON.stringify({ type: "pong" }));
      return;
    }
    if (role === "media-client" && peer?.readyState === WebSocket.OPEN) peer.send(text);
  });
  socket.once("close", (code, reason) => {
    emit({ type: "socket-close", kind: route.kind, run: route.run, role,
      code, reason: String(reason || "") });
    if (!role || room[role] !== socket) return;
    room[role] = null;
    notify(room[peerRole()], false);
    if (!room["media-host"] && !room["media-client"]) rooms.delete(route.key);
  });
  socket.on("error", error => emit({ type: "socket-error", kind: route.kind, run: route.run,
    role, message: String(error?.message || error) }));
}

// A single FIFO bottleneck for host->client media (lab only): each packet is
// serialized at `kbps` behind the ones before it, then delayed `delayMs`.
// Nothing is dropped, like a deep buffer in front of a slow link; the host
// sees the queue as growing ACK RTT.
export function createMediaShaper(initial, clock = () => performance.now(), schedule = setTimeout) {
  let config = null;
  let busyUntil = 0;
  let queuedBytes = 0;
  let maxQueueMs = 0;
  let packets = 0;
  const set = throttle => {
    if (!throttle) { config = null; return; }
    const kbps = Number(throttle.kbps);
    const delayMs = Number(throttle.delayMs ?? 0);
    if (!(kbps > 0) || !(delayMs >= 0)) throw new RangeError("throttle needs kbps > 0 and delayMs >= 0");
    config = { kbps, delayMs };
  };
  set(initial);
  return {
    set,
    stats: () => ({ ...config, packets, queuedBytes, maxQueueMs: +maxQueueMs.toFixed(1) }),
    /// Release time (ms, on `clock`) of a packet of `bytes` offered now.
    admit(bytes) {
      const now = clock();
      if (!config) return now;
      const start = Math.max(now, busyUntil);
      busyUntil = start + bytes * 8 / config.kbps;
      maxQueueMs = Math.max(maxQueueMs, busyUntil - now);
      packets++;
      return busyUntil + config.delayMs;
    },
    forward(peer, data) {
      const now = clock();
      if (!config) {
        peer.send(data, { binary: true });
        return;
      }
      const at = this.admit(data.byteLength);
      queuedBytes += data.byteLength;
      schedule(() => {
        queuedBytes -= data.byteLength;
        if (peer.readyState === WebSocket.OPEN) peer.send(data, { binary: true });
      }, Math.max(0, at - now));
    },
  };
}

function getRoom(rooms, key, create) {
  let room = rooms.get(key);
  if (!room) {
    room = create();
    rooms.set(key, room);
  }
  return room;
}

function replaceSocket(room, role, socket) {
  const existing = room[role];
  if (existing && existing !== socket) existing.close(1008, `${role} replaced`);
  room[role] = socket;
}

function parseJson(data) {
  try { return JSON.parse(String(data)); } catch { return null; }
}

function socketPath(rawUrl) {
  try { return new URL(rawUrl, "http://127.0.0.1").pathname; }
  catch { return null; }
}

async function readClientTrace(request) {
  const chunks = [];
  let bytes = 0;
  for await (const chunk of request) {
    bytes += chunk.byteLength;
    if (bytes > MAX_CLIENT_TRACE_BYTES) return null;
    chunks.push(chunk);
  }
  const trace = parseJson(Buffer.concat(chunks));
  if (!trace || trace.schemaVersion !== 1 || !UUID_PATTERN.test(trace.attemptId || "") ||
      (trace.runId !== null && !UUID_PATTERN.test(trace.runId || "")) ||
      typeof trace.stage !== "string") return null;
  return trace;
}

async function serveAsset(request, response, publicDir) {
  if (!["GET", "HEAD"].includes(request.method || "")) {
    response.writeHead(405, { Allow: "GET, HEAD" });
    response.end();
    return;
  }
  let pathname;
  try { pathname = decodeURIComponent(new URL(request.url, "http://127.0.0.1").pathname); }
  catch {
    response.writeHead(400);
    response.end();
    return;
  }
  const relative = normalize(pathname === "/" ? "index.html" : pathname.replace(/^[/\\]+/, ""));
  const path = resolve(publicDir, relative);
  if (isAbsolute(relative) || (path !== publicDir && !path.startsWith(`${publicDir}\\`) && !path.startsWith(`${publicDir}/`))) {
    response.writeHead(404);
    response.end();
    return;
  }
  try {
    const body = await readFile(path);
    response.writeHead(200, {
      "Cache-Control": "no-store",
      "Content-Length": body.byteLength,
      "Content-Type": MIME_TYPES.get(extname(path).toLowerCase()) || "application/octet-stream",
    });
    response.end(request.method === "HEAD" ? undefined : body);
  } catch {
    response.writeHead(404, { "Cache-Control": "no-store" });
    response.end("Not found");
  }
}

async function serveRequest(request, response, publicDir, emit) {
  let pathname;
  try { pathname = new URL(request.url, "http://127.0.0.1").pathname; }
  catch { pathname = null; }
  if (pathname === "/lab-compat" && ["GET", "HEAD"].includes(request.method || "")) {
    const body = Buffer.from(JSON.stringify({ ok: true, compatibility: STREAM_LAB_COMPATIBILITY }));
    response.writeHead(200, {
      "Cache-Control": "no-store",
      "Content-Length": body.byteLength,
      "Content-Type": "application/json; charset=utf-8",
    });
    response.end(request.method === "HEAD" ? undefined : body);
    return;
  }
  if (pathname === "/__diag/client" && request.method === "POST") {
    const trace = await readClientTrace(request);
    if (!trace) {
      response.writeHead(400, { "Cache-Control": "no-store" });
      response.end();
      return;
    }
    emit({ type: "client-trace", trace });
    response.writeHead(204, { "Cache-Control": "no-store" });
    response.end();
    return;
  }
  await serveAsset(request, response, publicDir);
}

function cliPort(args) {
  const index = args.indexOf("--port");
  if (index < 0) return 8787;
  return Number(args[index + 1]);
}

const isMain = process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url);
if (isMain) {
  const relay = await startDirectLabRelay({
    port: cliPort(process.argv.slice(2)),
    onEvent: event => process.stderr.write(`Stream Lab relay event ${JSON.stringify(event)}\n`),
  });
  process.stdout.write(`BubblePlay direct Stream Lab relay ready at ${relay.url}\n`);
  const stop = async () => {
    await relay.close();
    process.exit(0);
  };
  process.once("SIGINT", stop);
  process.once("SIGTERM", stop);
}
