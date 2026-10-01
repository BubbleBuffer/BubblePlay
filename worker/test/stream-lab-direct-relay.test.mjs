import assert from "node:assert/strict";
import test from "node:test";
import WebSocket from "ws";
import { STREAM_LAB_COMPATIBILITY } from "../public/media.js";
import { createMediaShaper, startDirectLabRelay } from "../tools/stream-lab-direct-relay.mjs";

const RUN_ID = "123e4567-e89b-42d3-a456-426614174000";
const EPOCH = "0123456789abcdef0123456789abcdef";
const COMPAT = `compat=${STREAM_LAB_COMPATIBILITY.id}`;

async function openRole(url, role) {
  const socket = new WebSocket(url);
  await new Promise((resolve, reject) => {
    socket.once("open", resolve);
    socket.once("error", reject);
  });
  socket.send(JSON.stringify({ type: "hello", role }));
  const hello = await nextJson(socket);
  assert.equal(hello.type, "hello-ok");
  return { socket, hello };
}

function nextMessage(socket) {
  return new Promise((resolve, reject) => {
    socket.once("message", resolve);
    socket.once("error", reject);
  });
}

async function nextJson(socket) {
  return JSON.parse(String(await nextMessage(socket)));
}

test("direct Stream Lab relay serves the existing receiver assets on loopback", async (t) => {
  const relay = await startDirectLabRelay({ port: 0 });
  t.after(() => relay.close());

  assert.equal(new URL(relay.url).hostname, "127.0.0.1");
  const response = await fetch(relay.url);
  assert.equal(response.status, 200);
  assert.match(await response.text(), /id="session-strip"/);
});

test("direct Stream Lab relay exposes the exact receiver compatibility contract", async (t) => {
  const relay = await startDirectLabRelay({ port: 0 });
  t.after(() => relay.close());

  const response = await fetch(new URL("/lab-compat", relay.url));
  assert.equal(response.status, 200);
  assert.equal(response.headers.get("cache-control"), "no-store");
  assert.deepEqual(await response.json(), {
    ok: true,
    compatibility: STREAM_LAB_COMPATIBILITY,
  });
});

test("direct Stream Lab relay records browser traces and signaling socket lifecycle", async (t) => {
  const events = [];
  let resolveServerClose;
  const serverClose = new Promise(resolve => { resolveServerClose = resolve; });
  const relay = await startDirectLabRelay({ port: 0, onEvent: event => {
    events.push(event);
    if (event.type === "socket-close" && event.role === "client") resolveServerClose(event);
  } });
  t.after(() => relay.close());

  const trace = {
    schemaVersion: 1,
    attemptId: RUN_ID,
    runId: RUN_ID,
    compatibilityId: STREAM_LAB_COMPATIBILITY.id,
    receiverBuild: STREAM_LAB_COMPATIBILITY.receiverBuild,
    seq: 1,
    atMs: Date.now(),
    stage: "signaling-construct",
    detail: { path: "/lab-ws", generation: 1 },
  };
  const response = await fetch(new URL("/__diag/client", relay.url), {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(trace),
  });
  assert.equal(response.status, 204);

  const signalBase = relay.url.replace(/^http/, "ws");
  const client = await openRole(`${signalBase}lab-ws?run=${RUN_ID}&${COMPAT}`, "client");
  client.socket.close(1000, "test-complete");
  await new Promise(resolve => client.socket.once("close", resolve));
  await serverClose;

  assert.deepEqual(events.find(event => event.type === "client-trace")?.trace, trace);
  assert.ok(events.some(event => event.type === "socket-upgrade" &&
    event.kind === "signal" && event.expectedRole === "client" && event.run === RUN_ID));
  assert.ok(events.some(event => event.type === "socket-hello" && event.role === "client"));
  assert.ok(events.some(event => event.type === "socket-close" &&
    event.role === "client" && event.code === 1000 && event.reason === "test-complete"));
});

test("direct Stream Lab signaling pairs one host and browser with the existing hello contract", async (t) => {
  const relay = await startDirectLabRelay({ port: 0 });
  t.after(() => relay.close());
  const signalBase = relay.url.replace(/^http/, "ws");
  const host = await openRole(`${signalBase}lab-host-ws?run=${RUN_ID}&${COMPAT}`, "host");
  const client = await openRole(`${signalBase}lab-ws?run=${RUN_ID}&${COMPAT}`, "client");
  t.after(() => host.socket.close());
  t.after(() => client.socket.close());

  assert.match(client.hello.mediaEpoch, /^[a-f0-9]{32}$/);
  assert.ok(Array.isArray(client.hello.iceServers));

  const capabilities = { type: "host-capabilities", generation: 7, wssMedia: true };
  host.socket.send(JSON.stringify(capabilities));
  assert.deepEqual(await nextJson(client.socket), capabilities);

  client.socket.send(JSON.stringify({ type: "ping" }));
  assert.deepEqual(await nextJson(client.socket), { type: "pong" });
});

test("direct Stream Lab relay rejects missing or stale compatibility identifiers", async (t) => {
  const relay = await startDirectLabRelay({ port: 0 });
  t.after(() => relay.close());
  const signalBase = relay.url.replace(/^http/, "ws");
  for (const query of [
    `run=${RUN_ID}`,
    `run=${RUN_ID}&compat=stale-compatible-label`,
  ]) {
    const socket = new WebSocket(`${signalBase}lab-host-ws?${query}`);
    await assert.rejects(new Promise((resolve, reject) => {
      socket.once("open", resolve);
      socket.once("error", reject);
      socket.once("unexpected-response", (_request, response) => {
        reject(new Error(`unexpected response ${response.statusCode}`));
      });
    }), /404|unexpected response/i);
    socket.terminate();
  }
});

function queued(socket) {
  const items = [];
  const waiters = [];
  socket.on("message", (data, isBinary) => {
    const item = { data, isBinary };
    const waiter = waiters.shift();
    if (waiter) waiter(item); else items.push(item);
  });
  return () => items.length ? Promise.resolve(items.shift()) : new Promise(resolve => waiters.push(resolve));
}

function v4Url(relay, path, epoch = EPOCH) {
  return `${relay.url.replace(/^http/, "ws")}${path}?epoch=${epoch}&run=${RUN_ID}&${COMPAT}`;
}

async function openV4(relay, role) {
  const socket = new WebSocket(v4Url(relay, role === "media-host" ? "lab-host-media-ws" : "lab-media-ws"));
  const next = queued(socket);
  await new Promise((resolve, reject) => { socket.once("open", resolve); socket.once("error", reject); });
  socket.send(JSON.stringify({ type: "hello", role }));
  assert.equal(JSON.parse(String((await next()).data)).type, "hello-ok");
  return { socket, next, json: async () => JSON.parse(String((await next()).data)) };
}

test("direct Stream Lab relay carries protocol v4 media, control, heartbeats and presence on one socket", async (t) => {
  const relay = await startDirectLabRelay({ port: 0 });
  t.after(() => relay.close());
  const { encodeFrameV4 } = await import("../public/media-v4.js");

  const client = await openV4(relay, "media-client");
  assert.deepEqual(await client.json(), { type: "media-peer", present: false });
  const host = await openV4(relay, "media-host");
  assert.deepEqual(await host.json(), { type: "media-peer", present: true });
  assert.deepEqual(await client.json(), { type: "media-peer", present: true });

  const packet = encodeFrameV4({ key: true, codec: 1, generation: 1, seq: 1, data: new Uint8Array([0, 0, 0, 1, 0x40]) });
  host.socket.send(packet);
  const media = await client.next();
  assert.equal(media.isBinary, true);
  assert.deepEqual([...new Uint8Array(media.data)], [...packet]);

  const ack = JSON.stringify({ type: "ack", generation: 1, seq: 1, recvTsMs: 5 });
  client.socket.send(ack);
  assert.equal(String((await host.next()).data), ack);
  client.socket.send("{\"type\":\"bogus\"}");
  client.socket.send(JSON.stringify({ type: "keyframe", reason: "sequence-gap" }));
  assert.deepEqual(await host.json(), { type: "keyframe", reason: "sequence-gap" });

  client.socket.send(JSON.stringify({ type: "ping" }));
  assert.deepEqual(await client.json(), { type: "pong" });
  host.socket.send(JSON.stringify({ type: "ping" }));
  assert.deepEqual(await host.json(), { type: "pong" });

  host.socket.close();
  assert.deepEqual(await client.json(), { type: "media-peer", present: false });
  client.socket.close();
});

test("direct Stream Lab relay rejects media sockets without a canonical epoch", async (t) => {
  const relay = await startDirectLabRelay({ port: 0 });
  t.after(() => relay.close());
  const socket = new WebSocket(v4Url(relay, "lab-media-ws", "BAD"));
  await assert.rejects(new Promise((resolve, reject) => {
    socket.once("open", resolve);
    socket.once("error", reject);
    socket.once("unexpected-response", (_request, response) => {
      reject(new Error(`unexpected response ${response.statusCode}`));
    });
  }), /404|unexpected response/i);
  socket.terminate();
});

test("lab media shaper serializes packets FIFO at the throttle rate", () => {
  let now = 0;
  const sent = [];
  const timers = [];
  const shaper = createMediaShaper({ kbps: 1000, delayMs: 20 }, () => now, (fn, ms) => timers.push({ fn, at: now + ms }));
  const peer = { readyState: WebSocket.OPEN, send: data => sent.push({ bytes: data.byteLength, at: now }) };
  // 1000 kbps = 1 byte per 8 us: 12500 bytes take 100 ms on the wire.
  shaper.forward(peer, new Uint8Array(12_500));
  shaper.forward(peer, new Uint8Array(12_500));
  assert.deepEqual(timers.map(t => t.at), [120, 220]);
  assert.equal(shaper.stats().maxQueueMs, 200);
  for (const timer of timers) { now = timer.at; timer.fn(); }
  assert.deepEqual(sent.map(s => s.at), [120, 220]);
  assert.equal(shaper.stats().queuedBytes, 0);
  shaper.set(null);
  shaper.forward(peer, new Uint8Array(10));
  assert.equal(sent.length, 3);
  assert.throws(() => shaper.set({ kbps: 0 }), RangeError);
});
