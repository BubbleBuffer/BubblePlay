import test from "node:test";
import assert from "node:assert/strict";
import { createTurnIceServers } from "../src/turn.js";
import { Room } from "../src/index.js";

const STUN_URL = "stun:stun.l.google.com:19302";

test("mints the coturn REST credential with a bounded deterministic expiry", async () => {
  const iceServers = await createTurnIceServers(
    { TURN_SECRET: "1234567890", TURN_TTL_SECONDS: "999999" },
    { now: () => 1_700_000_000_000 },
  );

  assert.deepEqual(iceServers, [
    { urls: STUN_URL },
    {
      urls: [
        "turn:turn.example.com:3478?transport=udp",
        "turn:turn.example.com:3478?transport=tcp",
      ],
      username: "1700086400:bubbleplay",
      credential: "SldKiYBI923K/9zxmmhPIaexMFY=",
    },
  ]);
});

test("uses STUN only when TURN_SECRET is missing or invalid", async () => {
  assert.deepEqual(await createTurnIceServers({}), [{ urls: STUN_URL }]);
  assert.deepEqual(await createTurnIceServers({ TURN_SECRET: "   " }), [{ urls: STUN_URL }]);
  assert.deepEqual(await createTurnIceServers({ TURN_SECRET: 42 }), [{ urls: STUN_URL }]);
});

class FakeSocket {
  constructor() {
    this.attachment = { role: null, queryToken: "" };
    this.readyState = 1;
    this.messages = [];
    this.closed = [];
  }
  deserializeAttachment() { return this.attachment; }
  serializeAttachment(value) { this.attachment = value; }
  send(message) { this.messages.push(message); }
  close(code, reason) { this.closed.push({ code, reason }); }
}

test("only authenticated client hello receives ephemeral TURN config", async () => {
  const storage = { values: new Map(), get: async (key) => storage.values.get(key), put: async (key, value) => storage.values.set(key, value), delete: async (key) => storage.values.delete(key) };
  const client = new FakeSocket();
  const host = new FakeSocket();
  const env = { TOKEN: "token", TURN_SECRET: "1234567890" };
  const room = new Room({ getWebSockets: () => [client, host], storage }, env);

  await room.webSocketMessage(host, JSON.stringify({ type: "hello", token: "token", role: "host" }));
  await room.webSocketMessage(client, JSON.stringify({ type: "hello", token: "token", role: "client" }));

  const hostHello = JSON.parse(host.messages[0]);
  const clientHello = JSON.parse(client.messages[0]);
  assert.deepEqual(hostHello, { type: "hello-ok" });
  assert.equal(clientHello.type, "hello-ok");
  assert.equal(clientHello.iceServers.length, 2);
  const observable = JSON.stringify({ messages: [...host.messages, ...client.messages], stored: [...storage.values.values()] });
  assert.equal(observable.includes(env.TURN_SECRET), false);
  assert.equal(observable.includes("turn:turn.example.com"), true);
});

test("never authenticates hello when TOKEN is absent, empty, or mismatched", async () => {
  for (const env of [
    { TURN_SECRET: "1234567890" },
    { TOKEN: "", TURN_SECRET: "1234567890" },
    { TOKEN: "configured", TURN_SECRET: "1234567890" },
  ]) {
    const storage = { get: async () => [], put: async () => {}, delete: async () => {} };
    const client = new FakeSocket();
    const room = new Room({ getWebSockets: () => [client], storage }, env);
    await room.webSocketMessage(client, JSON.stringify({ type: "hello", token: "wrong", role: "client" }));
    assert.equal(client.messages.some((message) => JSON.parse(message).type === "hello-ok"), false);
    assert.equal(client.messages.some((message) => message.includes("turn:turn.example.com")), false);
  }
});
