import test from "node:test";
import assert from "node:assert/strict";
import { Room } from "../src/index.js";

class FakeSocket {
  constructor(role, stripe, extra = {}) {
    const { throwOnSend = false, ...attachment } = extra;
    this.attachment = { role, stripe, ...attachment };
    this.readyState = 1;
    this.messages = [];
    this.closed = [];
    this.throwOnSend = throwOnSend;
  }
  deserializeAttachment() { return this.attachment; }
  serializeAttachment(value) { this.attachment = value; }
  send(value) {
    if (this.throwOnSend) throw new Error("socket is closing");
    this.messages.push(value);
  }
  close(code, reason) { this.closed.push({ code, reason }); }
}

test("a media host forwards only bounded binary packets to the matching client stripe", async () => {
  const host = new FakeSocket("media-host", 2);
  const client = new FakeSocket("media-client", 2);
  const wrongStripe = new FakeSocket("media-client", 1);
  const room = new Room({ getWebSockets: () => [host, client, wrongStripe] }, {});
  const frame = new Uint8Array(49_176).buffer;
  await room.webSocketMessage(host, frame);
  assert.deepEqual(client.messages, [frame]);
  assert.deepEqual(wrongStripe.messages, []);
});

test("lab v3 media appends one bounded edge-arrival timestamp without changing production packets", async () => {
  const run = "123e4567-e89b-42d3-a456-426614174000";
  const host = new FakeSocket("media-host", 2, { run, version: 3 });
  const client = new FakeSocket("media-client", 2, { run, version: 3, registeredAtMs: 123_456 });
  const room = new Room({ getWebSockets: () => [host, client] }, {});
  const packetBytes = new Uint8Array(19);
  packetBytes.set([3, 1, 0, 2, 9, 8, 7]);
  const hostTrailer = new DataView(packetBytes.buffer, 7, 12);
  hostTrailer.setUint32(0, 0x42504854);
  hostTrailer.setFloat64(4, 1_787_940_000_123);
  const packet = packetBytes.buffer;
  const earliest = Date.now();

  await room.webSocketMessage(host, packet);

  const latest = Date.now();
  assert.equal(client.messages.length, 2);
  assert.deepEqual(JSON.parse(client.messages[0]), {
    type: "media-forward",
    registeredAtMs: 123_456,
  });
  const forwarded = new Uint8Array(client.messages[1]);
  assert.deepEqual([...forwarded.subarray(0, 19)], [...packetBytes]);
  assert.equal(forwarded.byteLength, 31);
  const edgeTrailer = new DataView(forwarded.buffer, forwarded.byteOffset + 19, 12);
  assert.equal(edgeTrailer.getUint32(0), 0x42504554);
  assert.ok(edgeTrailer.getFloat64(4) >= earliest);
  assert.ok(edgeTrailer.getFloat64(4) <= latest);

  const productionHost = new FakeSocket("media-host", 2, { version: 3 });
  const productionClient = new FakeSocket("media-client", 2, { version: 3 });
  const productionRoom = new Room({ getWebSockets: () => [productionHost, productionClient] }, {});
  await productionRoom.webSocketMessage(productionHost, packet);
  assert.equal(productionClient.messages[0], packet);
});

test("lab forwarding announces a replacement media-client registration before its first packet", async () => {
  const run = "123e4567-e89b-42d3-a456-426614174000";
  const host = new FakeSocket("media-host", 12, { run, version: 3 });
  const first = new FakeSocket("media-client", 12, { run, version: 3, registeredAtMs: 10_001 });
  let sockets = [host, first];
  const room = new Room({ getWebSockets: () => sockets }, {});
  const firstPacket = Uint8Array.of(1).buffer;
  await room.webSocketMessage(host, firstPacket);

  first.readyState = 3;
  const replacement = new FakeSocket("media-client", 12, { run, version: 3, registeredAtMs: 10_002 });
  sockets = [host, first, replacement];
  const replacementPacket = Uint8Array.of(2).buffer;
  await room.webSocketMessage(host, replacementPacket);

  assert.deepEqual(JSON.parse(first.messages[0]), {
    type: "media-forward",
    registeredAtMs: 10_001,
  });
  assert.equal(first.messages[1] instanceof ArrayBuffer, true);
  assert.deepEqual(JSON.parse(replacement.messages[0]), {
    type: "media-forward",
    registeredAtMs: 10_002,
  });
  const forwardedReplacement = new Uint8Array(replacement.messages[1]);
  assert.equal(forwardedReplacement.byteLength, 13);
  assert.equal(forwardedReplacement[0], 2);
});

test("repeated media packets reuse the validated stripe peer without rescanning room sockets", async () => {
  const host = new FakeSocket("media-host", 2);
  const client = new FakeSocket("media-client", 2);
  let enumerations = 0;
  const room = new Room({ getWebSockets: () => {
    enumerations += 1;
    return [host, client];
  } }, {});
  const first = Uint8Array.of(1).buffer;
  const second = Uint8Array.of(2).buffer;

  await room.webSocketMessage(host, first);
  await room.webSocketMessage(host, second);

  assert.deepEqual(client.messages, [first, second]);
  assert.equal(enumerations, 1);
});

test("a closed or replaced cached stripe peer is never reused", async () => {
  const host = new FakeSocket("media-host", 2);
  const firstClient = new FakeSocket("media-client", 2);
  const replacement = new FakeSocket("media-client", 2);
  let sockets = [host, firstClient];
  const state = {
    getWebSockets: () => sockets,
    storage: { delete: async () => {} },
  };
  const room = new Room(state, {});
  const first = Uint8Array.of(1).buffer;
  const second = Uint8Array.of(2).buffer;
  const third = Uint8Array.of(3).buffer;

  await room.webSocketMessage(host, first);
  await room.webSocketClose(firstClient, 1006, "lost", false);
  sockets = [host, replacement];
  await room.webSocketMessage(host, second);
  room.reject(replacement, "media-client replaced");
  const latest = new FakeSocket("media-client", 2);
  sockets = [host, replacement, latest];
  await room.webSocketMessage(host, third);

  assert.deepEqual(firstClient.messages, [first]);
  assert.deepEqual(replacement.messages, [second, JSON.stringify({ type: "error", message: "media-client replaced" })]);
  assert.deepEqual(latest.messages, [third]);
});

test("a re-instantiated room rebuilds its peer cache from hibernated sockets", async () => {
  const host = new FakeSocket("media-host", 2);
  const firstClient = new FakeSocket("media-client", 2);
  let sockets = [host, firstClient];
  const state = { getWebSockets: () => sockets };
  const firstRoom = new Room(state, {});
  const first = Uint8Array.of(1).buffer;
  await firstRoom.webSocketMessage(host, first);

  const resumedClient = new FakeSocket("media-client", 2);
  sockets = [host, resumedClient];
  const resumedRoom = new Room(state, {});
  const second = Uint8Array.of(2).buffer;
  await resumedRoom.webSocketMessage(host, second);

  assert.deepEqual(firstClient.messages, [first]);
  assert.deepEqual(resumedClient.messages, [second]);
});

test("a replacement remains authoritative when rejecting the stale socket cannot send", async () => {
  const host = new FakeSocket("media-host", 14);
  const staleClient = new FakeSocket("media-client", 14, { throwOnSend: true });
  const replacement = new FakeSocket(null, 14, { expectedRole: "media-client" });
  const sockets = [host, staleClient, replacement];
  const state = {
    getWebSockets: () => sockets,
    storage: { delete: async () => {} },
  };

  const registeringRoom = new Room(state, {});
  await registeringRoom.webSocketMessage(
    replacement,
    JSON.stringify({ type: "hello", role: "media-client" }),
  );
  const replacementHello = replacement.messages[0];
  const parsedHello = JSON.parse(replacementHello);
  assert.equal(parsedHello.type, "hello-ok");
  assert.equal(Number.isSafeInteger(parsedHello.registeredAtMs), true);

  const resumedRoom = new Room(state, {});
  const frame = Uint8Array.of(7).buffer;
  await resumedRoom.webSocketMessage(host, frame);

  assert.equal(staleClient.attachment.replaced, true);
  assert.deepEqual(replacement.messages, [replacementHello, frame]);
});

test("media clients cannot inject video and oversized host frames are discarded", async () => {
  const host = new FakeSocket("media-host", 0);
  const client = new FakeSocket("media-client", 0);
  const room = new Room({ getWebSockets: () => [host, client] }, {});
  await room.webSocketMessage(client, new Uint8Array(8).buffer);
  await room.webSocketMessage(host, new Uint8Array(49_177).buffer);
  assert.deepEqual(host.messages, []);
  assert.deepEqual(client.messages, []);
});

test("v3 relay accepts a bounded one-data-lane packet larger than the fixed v2 shard cap", async () => {
  const epoch = "0123456789abcdef0123456789abcdef";
  const host = new FakeSocket("media-host", 0, { epoch, version: 3 });
  const client = new FakeSocket("media-client", 0, { epoch, version: 3 });
  const room = new Room({ getWebSockets: () => [host, client] }, {});
  const packet = new Uint8Array(3 * 1024 * 1024).buffer;
  await room.webSocketMessage(host, packet);
  assert.equal(client.messages.length, 1);
  assert.equal(client.messages[0], packet);
  await room.webSocketMessage(host, new Uint8Array(8 * 1024 * 1024 + 29).buffer);
  assert.equal(client.messages.length, 1);
});

test("closing a media stripe does not tear down the signaling peer", async () => {
  const media = new FakeSocket("media-client", 3);
  const host = new FakeSocket("host", null);
  const state = { getWebSockets: () => [media, host], storage: { delete: async () => {} } };
  const room = new Room(state, {});
  await room.webSocketClose(media, 1006, "lost", false);
  assert.deepEqual(host.messages, []);
});

test("a WSS start command waits for a temporarily absent host", async () => {
  const client = new FakeSocket("client", null);
  let pending = [];
  const state = {
    getWebSockets: () => [client],
    storage: {
      get: async () => pending,
      put: async (_key, value) => { pending = value; },
      delete: async () => { pending = []; },
    },
  };
  const room = new Room(state, {});
  const start = JSON.stringify({ type: "media-control", action: "start", codec: "hevc" });
  await room.webSocketMessage(client, start);
  assert.deepEqual(pending, [start]);
});

test("replacing a signaling host forces the live client to establish a fresh session", async () => {
  const staleHost = new FakeSocket("host", null, { registeredAtMs: 10_001 });
  const client = new FakeSocket("client", null, { registeredAtMs: 10_002 });
  const replacement = new FakeSocket(null, null, { expectedRole: "host" });
  const sockets = [staleHost, client, replacement];
  const state = {
    getWebSockets: () => sockets,
    storage: {
      get: async () => [],
      delete: async () => {},
    },
  };
  const room = new Room(state, { TOKEN: "test-token" });

  await room.webSocketMessage(replacement, JSON.stringify({
    type: "hello",
    role: "host",
    token: "test-token",
  }));

  assert.deepEqual(client.messages, [JSON.stringify({ type: "peer-left" })]);
  assert.deepEqual(staleHost.closed, [{ code: 1008, reason: "host replaced" }]);
  assert.equal(replacement.attachment.role, "host");
});
