import assert from "node:assert/strict";
import test from "node:test";
import WebSocket from "ws";
import { STREAM_LAB_COMPATIBILITY } from "../public/media.js";
import { startDirectLabRelay } from "../tools/stream-lab-direct-relay.mjs";

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
  assert.match(await response.text(), /id="app-brand"/);
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

test("direct Stream Lab media preserves the host trailer and appends one edge trailer", async (t) => {
  const relay = await startDirectLabRelay({ port: 0 });
  t.after(() => relay.close());
  const mediaBase = relay.url.replace(/^http/, "ws");
  const query = `v=3&epoch=${EPOCH}&stripe=15&run=${RUN_ID}&${COMPAT}`;
  const host = await openRole(`${mediaBase}lab-host-media-ws?${query}`, "media-host");
  const client = await openRole(`${mediaBase}lab-media-ws?${query}`, "media-client");
  t.after(() => host.socket.close());
  t.after(() => client.socket.close());

  const packet = new Uint8Array(19);
  packet.set([3, 1, 0, 15, 9, 8, 7]);
  const hostTrailer = new DataView(packet.buffer, 7, 12);
  hostTrailer.setUint32(0, 0x42504854);
  hostTrailer.setFloat64(4, 1_787_940_000_100);
  const earliest = Date.now();
  host.socket.send(packet);

  const forwarded = new Uint8Array(await nextMessage(client.socket));
  const latest = Date.now();
  assert.equal(forwarded.byteLength, packet.byteLength + 12);
  assert.deepEqual([...forwarded.subarray(0, packet.byteLength)], [...packet]);
  const edgeTrailer = new DataView(forwarded.buffer, forwarded.byteOffset + packet.byteLength, 12);
  assert.equal(edgeTrailer.getUint32(0), 0x42504554);
  assert.ok(edgeTrailer.getFloat64(4) >= earliest);
  assert.ok(edgeTrailer.getFloat64(4) <= latest);
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
