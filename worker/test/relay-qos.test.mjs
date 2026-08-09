import test from "node:test";
import assert from "node:assert/strict";
import { Room } from "../src/index.js";

class FakeSocket {
  constructor(role) {
    this.attachment = { role };
    this.readyState = 1;
    this.messages = [];
    this.closed = [];
  }

  deserializeAttachment() { return this.attachment; }
  serializeAttachment(value) { this.attachment = value; }
  send(message) { this.messages.push(message); }
  close(code, reason) { this.closed.push({ code, reason }); }
}

function createRoom() {
  const sockets = [new FakeSocket("client"), new FakeSocket("host")];
  const storage = { get: async () => [], put: async () => {}, delete: async () => {} };
  return { room: new Room({ getWebSockets: () => sockets, storage }, { TOKEN: "test" }), sockets };
}

test("relays bounded QoS client-to-host and quality host-to-client only", async () => {
  const { room, sockets: [client, host] } = createRoom();
  const qos = JSON.stringify({ type: "qos", sessionId: "page-random", seq: 1, rx: { mbps: 4.2 } });
  const quality = JSON.stringify({ type: "quality", profile: "balanced", width: 1280, height: 720, fps: 30 });

  await room.webSocketMessage(client, qos);
  await room.webSocketMessage(host, quality);

  assert.deepEqual(host.messages, [qos]);
  assert.deepEqual(client.messages, [quality]);
  assert.deepEqual(client.closed, []);
  assert.deepEqual(host.closed, []);
});

test("drops wrong-role and oversize QoS/quality frames without closing peers", async () => {
  const { room, sockets: [client, host] } = createRoom();
  const oversizeQos = JSON.stringify({ type: "qos", payload: "x".repeat(2_048) });
  const oversizeQuality = JSON.stringify({ type: "quality", payload: "x".repeat(1_024) });

  await room.webSocketMessage(host, JSON.stringify({ type: "qos" }));
  await room.webSocketMessage(client, JSON.stringify({ type: "quality" }));
  await room.webSocketMessage(client, oversizeQos);
  await room.webSocketMessage(host, oversizeQuality);

  assert.deepEqual(client.messages, []);
  assert.deepEqual(host.messages, []);
  assert.deepEqual(client.closed, []);
  assert.deepEqual(host.closed, []);
});
