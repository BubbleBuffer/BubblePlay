import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import worker, { Room } from "../src/index.js";
import { RELAY_SECRETS } from "./helpers/relay-secrets.mjs";
import {
  V4_HEADER_BYTES,
  V4_MAX_PAYLOAD_BYTES,
  V4ReceiveGate,
  ackMessageV4,
  encodeFrameV4,
  keyframeMessageV4,
  nextBackoffMsV4,
  parseFrameV4,
  shouldPresentDecodedV4,
  validateClientMessageV4,
  validateHostTextV4,
  wssV4StatusLabel,
} from "../public/media-v4.js";

const contract = JSON.parse(readFileSync(new URL("../../fixtures/wss-v4-contract.json", import.meta.url), "utf8"));
const hex = value => Uint8Array.from(value.match(/../g) || [], byte => parseInt(byte, 16));
const toHex = bytes => Array.from(bytes, byte => byte.toString(16).padStart(2, "0")).join("");

class FakeSocket {
  constructor(role, stripe, extra = {}) {
    this.attachment = { role, stripe, ...extra };
    this.readyState = 1;
    this.messages = [];
    this.closed = [];
  }
  deserializeAttachment() { return this.attachment; }
  serializeAttachment(value) { this.attachment = value; }
  send(value) { this.messages.push(value); }
  close(code, reason) { this.closed.push({ code, reason }); }
}

test("v4 packets match the shared host/receiver fixture", () => {
  assert.equal(contract.headerBytes, V4_HEADER_BYTES);
  assert.equal(contract.maxPayloadBytes, V4_MAX_PAYLOAD_BYTES);
  for (const packet of contract.packets) {
    const bytes = hex(packet.hex);
    const frame = parseFrameV4(bytes.buffer);
    assert.equal(frame !== null, packet.accepted, packet.name);
    if (!packet.accepted) continue;
    assert.equal(frame.key, packet.key);
    assert.equal(frame.refine, packet.refine);
    assert.equal(frame.codec, packet.codec);
    assert.equal(frame.generation, packet.generation);
    assert.equal(frame.seq, packet.seq);
    assert.equal(frame.captureTsUs, BigInt(packet.captureTsUs));
    assert.equal(frame.width, packet.width);
    assert.equal(frame.height, packet.height);
    assert.equal(toHex(frame.data), packet.payloadHex);
    assert.equal(toHex(encodeFrameV4({ ...frame, captureTsUs: frame.captureTsUs })), packet.hex);
  }
});

test("v4 client messages follow the shared validation contract", () => {
  for (const message of contract.clientMessages) {
    const parsed = validateClientMessageV4(message.text);
    assert.equal(parsed !== null, message.accepted, message.text);
    if (message.kind) assert.equal(parsed.type, message.kind);
  }
  for (const message of contract.hostMessages) {
    assert.equal(validateHostTextV4(message.text) !== null, message.accepted, message.text);
  }
  assert.equal(validateClientMessageV4(`{"type":"keyframe","reason":"${"a".repeat(600)}"}`), null);
});

test("v4 message builders produce relay-valid text", () => {
  const ack = ackMessageV4({ generation: 7, seq: 42, recvTsMs: 1234.56, decodedSeq: 41 });
  assert.deepEqual(validateClientMessageV4(ack), {
    type: "ack", generation: 7, seq: 42, recvTsMs: 1234.6, decodedSeq: 41,
  });
  assert.ok(validateClientMessageV4(ackMessageV4({ generation: 1, seq: 2, recvTsMs: 3 })));
  assert.deepEqual(JSON.parse(keyframeMessageV4("decoder error!")), { type: "keyframe", reason: "decoder-error-" });
  assert.ok(validateClientMessageV4(keyframeMessageV4("x".repeat(90))));
});

test("v4 routes accept one epoch-scoped socket per side and nothing else", async () => {
  const selected = [];
  const mediaResponse = new Response("media", { status: 200 });
  const env = {
    ...RELAY_SECRETS,
    APPROVAL_METHODS: "host",
    GATE: { idFromName: () => "gate", get: () => ({ fetch: async (request) => (new URL(request.url).pathname === "/__pc/resolve"
      ? Response.json({ pc: null }) : new Response(null, { status: 204 })) }) },
    ROOM: { idFromName: name => name, get: () => ({ fetch: async () => new Response("signal") }) },
    MEDIA_ROOM: { idFromName: name => { selected.push(name); return name; }, get: () => ({ fetch: async () => mediaResponse }) },
  };
  const epoch = "0123456789abcdef0123456789abcdef";
  const hostHeaders = { Upgrade: "websocket", Authorization: `Bearer ${RELAY_SECRETS.HOST_GATE_TOKEN}` };
  const browserHeaders = { Upgrade: "websocket" };
  assert.equal(await worker.fetch(new Request(
    `https://playfull-cat.example/host-media-ws?epoch=${epoch}`, { headers: hostHeaders }), env), mediaResponse);
  assert.equal(await worker.fetch(new Request(
    `https://playfull-cat.example/media-ws?epoch=${epoch}`, { headers: browserHeaders }), env), mediaResponse);
  assert.deepEqual(selected, [`media-v4:${epoch}`, `media-v4:${epoch}`]);
  for (const url of [
    "https://playfull-cat.example/host-media-ws",
    "https://playfull-cat.example/host-media-ws?epoch=BAD",
    `https://playfull-cat.example/host-media-ws?epoch=${epoch.toUpperCase()}`,
  ]) assert.equal((await worker.fetch(new Request(url, { headers: hostHeaders }), env)).status, 404, url);
  assert.equal(selected.length, 2);
});

test("v4 relay forwards frames host->client and validated control client->host on one socket", async () => {
  const host = new FakeSocket("media-host", 0, { version: 4, epoch: "e" });
  const client = new FakeSocket("media-client", 0, { version: 4, epoch: "e" });
  const room = new Room({ getWebSockets: () => [host, client] }, {});
  const frame = encodeFrameV4({ key: true, codec: 1, generation: 1, seq: 1, data: Uint8Array.of(1, 2, 3) }).buffer;
  await room.webSocketMessage(host, frame);
  assert.deepEqual(client.messages, [frame]);

  // Header-only and oversized packets never reach the receiver.
  await room.webSocketMessage(host, new Uint8Array(V4_HEADER_BYTES).buffer);
  await room.webSocketMessage(host, new Uint8Array(V4_HEADER_BYTES + V4_MAX_PAYLOAD_BYTES + 1).buffer);
  // The client may not send binary media to the host.
  await room.webSocketMessage(client, frame);
  assert.equal(client.messages.length, 1);
  assert.equal(host.messages.length, 0);

  const ack = ackMessageV4({ generation: 1, seq: 1, recvTsMs: 10 });
  await room.webSocketMessage(client, ack);
  await room.webSocketMessage(client, keyframeMessageV4("sequence-gap"));
  await room.webSocketMessage(client, '{"type":"viewport","x":1}');
  await room.webSocketMessage(client, "not json");
  assert.deepEqual(host.messages, [ack, '{"type":"keyframe","reason":"sequence-gap"}']);

  // Heartbeats are answered by the relay and never relayed, in both directions.
  await room.webSocketMessage(client, '{"type":"ping"}');
  await room.webSocketMessage(host, '{"type":"ping"}');
  assert.equal(client.messages.at(-1), '{"type":"pong"}');
  assert.equal(host.messages.at(-1), '{"type":"pong"}');
  assert.equal(host.messages.length, 3);
  // Host text is never relayed to the browser.
  await room.webSocketMessage(host, ack);
  assert.equal(client.messages.length, 2);
});

test("v4 relay drops traffic without a live peer", async () => {
  const host = new FakeSocket("media-host", 0, { version: 4 });
  const room = new Room({ getWebSockets: () => [host] }, {});
  const frame = encodeFrameV4({ codec: 2, generation: 1, seq: 2, data: Uint8Array.of(9) }).buffer;
  await room.webSocketMessage(host, frame);
  assert.deepEqual(host.messages, []);
});

test("v4 hello and close announce peer presence to the other side", async () => {
  const host = new FakeSocket(null, 0, { version: 4, expectedRole: "media-host" });
  let sockets = [host];
  const room = new Room({ getWebSockets: () => sockets }, {});
  await room.webSocketMessage(host, JSON.stringify({ type: "hello", role: "media-host" }));
  assert.deepEqual(host.messages.map(message => JSON.parse(message)), [
    { type: "hello-ok" },
    { type: "media-peer", present: false },
  ]);
  const client = new FakeSocket(null, 0, { version: 4, expectedRole: "media-client" });
  sockets = [host, client];
  await room.webSocketMessage(client, JSON.stringify({ type: "hello", role: "media-client" }));
  assert.deepEqual(JSON.parse(host.messages.at(-1)), { type: "media-peer", present: true });
  assert.deepEqual(JSON.parse(client.messages.at(-1)), { type: "media-peer", present: true });

  client.readyState = 3;
  sockets = [host];
  await room.webSocketClose(client, 1006, "", false);
  assert.deepEqual(JSON.parse(host.messages.at(-1)), { type: "media-peer", present: false });
});

test("v4 receive gate decodes in order and drops deltas until a keyframe after any gap", () => {
  const gate = new V4ReceiveGate();
  const frame = (seq, key = false, generation = 1) => ({ seq, key, generation });
  assert.deepEqual(gate.accept(frame(1)), { decode: false, requestKeyframe: "connect" });
  assert.deepEqual(gate.accept(frame(2, true)), { decode: true, requestKeyframe: null });
  assert.deepEqual(gate.accept(frame(3)), { decode: true, requestKeyframe: null });
  assert.deepEqual(gate.accept(frame(5)), { decode: false, requestKeyframe: "sequence-gap" });
  assert.equal(gate.accept(frame(6)).decode, false);
  assert.equal(gate.accept(frame(7, true)).decode, true);
  assert.equal(gate.accept(frame(8)).decode, true);
  gate.invalidate("decoder-error");
  assert.deepEqual(gate.accept(frame(9)), { decode: false, requestKeyframe: "decoder-error" });
  assert.equal(gate.accept(frame(10, true)).decode, true);
  assert.deepEqual(gate.accept(frame(11, false, 2)), { decode: false, requestKeyframe: "generation-change" });
  assert.equal(gate.accept(frame(0, true, 2)).decode, true);
  assert.equal(gate.accept(frame(1, false, 2)).decode, true);
  const wrap = new V4ReceiveGate();
  wrap.accept(frame(0xffff_ffff, true));
  assert.equal(wrap.accept(frame(0)).decode, true);
});

test("v4 reconnect backoff, presentation skipping and status labels", () => {
  const delays = [];
  let delay = 0;
  for (let i = 0; i < 7; i += 1) { delay = nextBackoffMsV4(delay); delays.push(delay); }
  assert.deepEqual(delays, [250, 500, 1000, 2000, 4000, 5000, 5000]);
  assert.equal(shouldPresentDecodedV4({ decodeQueueSize: 0, msSinceLastPresent: 5 }), true);
  assert.equal(shouldPresentDecodedV4({ decodeQueueSize: 1, msSinceLastPresent: 5 }), true);
  assert.equal(shouldPresentDecodedV4({ decodeQueueSize: 3, msSinceLastPresent: 5 }), false);
  assert.equal(shouldPresentDecodedV4({ decodeQueueSize: 3, msSinceLastPresent: 150 }), true);
  assert.equal(wssV4StatusLabel({ socketOpen: false, reconnecting: true }), "reconnecting");
  assert.equal(wssV4StatusLabel({ socketOpen: true, framesPresented: 0 }), "starting");
  assert.equal(wssV4StatusLabel({ socketOpen: true, framesPresented: 5, lastFrameAtMs: 0, nowMs: 5_000 }), "paused-static");
  assert.equal(wssV4StatusLabel({ socketOpen: true, framesPresented: 5, lastFrameAtMs: 4_900, nowMs: 5_000 }), "connected");
});

test("receiver wires v4: media-socket keyframes, in-order decode, reconnect backoff, static status", () => {
  const source = readFileSync(new URL("../public/receiver.js", import.meta.url), "utf8");
  assert.match(source, /from "\/media-v4\.js";/);
  assert.match(source, /const WSS_PROTOCOL = V4_VERSION;/);
  assert.match(source, /if \(usingWssMedia && !requestId\) return sendWssV4\(keyframeMessageV4\(reason\)\);/);
  assert.match(source, /wssV4\.backoffMs = nextBackoffMsV4\(wssV4\.backoffMs\);/);
  assert.match(source, /latencyGuard\.admitDecode\(usingWssMedia \? Infinity : decodeHardLimit\)/);
  assert.match(source, /shouldPresentDecodedV4\(/);
  assert.match(source, /Connected · WSS \(static\)/);
  assert.match(source, /if \(signalingRegistered\) sendRelay\(\{ type: "ping" \}\);\s*\}, V4_HEARTBEAT_MS\);/);
  // v4 has no deadline expiry timer.
  assert.match(source, /\/\/ v4 decodes every frame in order: there is no deadline expiry\.\s*startWssV4\(\);/);
});

test("receiver input stays on the signaling socket and the media socket carries only v4 control", () => {
  const source = readFileSync(new URL("../public/receiver.js", import.meta.url), "utf8").replace(/\r\n/g, "\n");
  const body = name => {
    const start = source.indexOf(`function ${name}(`);
    assert.ok(start >= 0, name);
    return source.slice(start, source.indexOf("\n}\n", start));
  };
  // Input goes to the RTC data channel or, under WSS, to the signaling socket.
  const sendInput = body("sendInput");
  assert.match(sendInput, /ws\.send\(data\)/);
  assert.doesNotMatch(sendInput, /wssV4|sendWssV4/);
  assert.match(body("inputControlState"), /ws\.bufferedAmount/);
  assert.doesNotMatch(body("inputControlState"), /wssV4/);
  // Every media-socket send is an ACK, keyframe request, heartbeat or hello.
  const calls = [...source.matchAll(/sendWssV4\(([^)]*)/g)].map(match => match[1]).filter(arg => arg !== "text");
  assert.equal(calls.length, 3);
  for (const arg of calls) assert.match(arg, /^(ackMessageV4|keyframeMessageV4|'\{"type":"ping"\}')/);
  // Shared media sender, plus the media hello with or without a signaling ticket.
  assert.equal([...source.matchAll(/(?<!ws)\bsocket\.send\(/g)].length, 3);
  assert.match(source, /socket\.send\(JSON\.stringify\(\{ type: "hello", role: "media-viewer", ticket: signalingTicket \}\)\)/);
  assert.match(source, /socket\.send\(JSON\.stringify\(\{ type: "hello", role: "media-viewer" \}\)\)/);
});
