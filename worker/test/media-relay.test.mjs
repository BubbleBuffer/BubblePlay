import test from "node:test";
import assert from "node:assert/strict";
import { Room } from "../src/index.js";
import { V4_HEADER_BYTES, V4_MAX_PACKET_BYTES } from "../public/media-v4.js";

class FakeSocket {
  constructor(role, extra = {}) {
    const { throwOnSend = false, ...attachment } = extra;
    this.attachment = { role, ...attachment };
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

function packet(marker) {
  const bytes = new Uint8Array(V4_HEADER_BYTES + 4);
  bytes[0] = marker;
  return bytes.buffer;
}

test("a media host forwards bounded v4 binary packets to the media client unchanged", async () => {
  const host = new FakeSocket("media-host");
  const client = new FakeSocket("media-client");
  const room = new Room({ getWebSockets: () => [host, client] }, {});
  const frame = new Uint8Array(V4_MAX_PACKET_BYTES).buffer;
  await room.webSocketMessage(host, frame);
  assert.deepEqual(client.messages, [frame]);
});

test("repeated media packets reuse the validated peer without rescanning room sockets", async () => {
  const host = new FakeSocket("media-host");
  const client = new FakeSocket("media-client");
  let enumerations = 0;
  const room = new Room({ getWebSockets: () => {
    enumerations += 1;
    return [host, client];
  } }, {});
  const first = packet(1);
  const second = packet(2);

  await room.webSocketMessage(host, first);
  await room.webSocketMessage(host, second);

  assert.deepEqual(client.messages, [first, second]);
  assert.equal(enumerations, 1);
});

test("a closed or replaced cached media peer is never reused", async () => {
  const host = new FakeSocket("media-host");
  const firstClient = new FakeSocket("media-client");
  const replacement = new FakeSocket("media-client");
  let sockets = [host, firstClient];
  const state = {
    getWebSockets: () => sockets,
    storage: { delete: async () => {} },
  };
  const room = new Room(state, {});
  const first = packet(1);
  const second = packet(2);
  const third = packet(3);

  await room.webSocketMessage(host, first);
  await room.webSocketClose(firstClient, 1006, "lost", false);
  sockets = [host, replacement];
  await room.webSocketMessage(host, second);
  room.reject(replacement, "media-client replaced");
  const latest = new FakeSocket("media-client");
  sockets = [host, replacement, latest];
  await room.webSocketMessage(host, third);

  assert.deepEqual(firstClient.messages, [first]);
  assert.deepEqual(replacement.messages, [second, JSON.stringify({ type: "error", message: "media-client replaced" })]);
  assert.deepEqual(latest.messages, [third]);
});

test("a re-instantiated room rebuilds its peer cache from hibernated sockets", async () => {
  const host = new FakeSocket("media-host");
  const firstClient = new FakeSocket("media-client");
  let sockets = [host, firstClient];
  const state = { getWebSockets: () => sockets };
  const firstRoom = new Room(state, {});
  const first = packet(1);
  await firstRoom.webSocketMessage(host, first);

  const resumedClient = new FakeSocket("media-client");
  sockets = [host, resumedClient];
  const resumedRoom = new Room(state, {});
  const second = packet(2);
  await resumedRoom.webSocketMessage(host, second);

  assert.deepEqual(firstClient.messages, [first]);
  assert.deepEqual(resumedClient.messages, [second]);
});

test("a replacement remains authoritative when rejecting the stale socket cannot send", async () => {
  const host = new FakeSocket("media-host");
  const staleClient = new FakeSocket("media-client", { throwOnSend: true });
  const replacement = new FakeSocket(null, { expectedRole: "media-client" });
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
  const frame = packet(7);
  await resumedRoom.webSocketMessage(host, frame);

  assert.equal(staleClient.attachment.replaced, true);
  assert.deepEqual(replacement.messages, [replacementHello, JSON.stringify({ type: "media-peer", present: true }), frame]);
});

test("media clients cannot inject video and header-only or oversized host frames are discarded", async () => {
  const host = new FakeSocket("media-host");
  const client = new FakeSocket("media-client");
  const room = new Room({ getWebSockets: () => [host, client] }, {});
  await room.webSocketMessage(client, packet(1));
  await room.webSocketMessage(host, new Uint8Array(V4_HEADER_BYTES).buffer);
  await room.webSocketMessage(host, new Uint8Array(V4_MAX_PACKET_BYTES + 1).buffer);
  assert.deepEqual(host.messages, []);
  assert.deepEqual(client.messages, []);
});

test("closing the media socket does not tear down the signaling peer", async () => {
  const media = new FakeSocket("media-client");
  const host = new FakeSocket("host");
  const state = { getWebSockets: () => [media, host], storage: { delete: async () => {} } };
  const room = new Room(state, {});
  await room.webSocketClose(media, 1006, "lost", false);
  assert.deepEqual(host.messages, []);
});

test("a WSS start command waits for a temporarily absent host", async () => {
  const client = new FakeSocket("client");
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
  const staleHost = new FakeSocket("host", { registeredAtMs: 10_001 });
  const client = new FakeSocket("client", { registeredAtMs: 10_002 });
  const replacement = new FakeSocket(null, { expectedRole: "host" });
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
