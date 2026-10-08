import test from "node:test";
import assert from "node:assert/strict";
import { createTurnIceServers } from "../src/turn.js";
import workerRuntime, { Room } from "../src/index.js";
import { RELAY_SECRETS } from "./helpers/relay-secrets.mjs";

const STUN_URL = "stun:stun.l.google.com:19302";

test("mints the coturn REST credential with a bounded deterministic expiry", async () => {
  const iceServers = await createTurnIceServers(
    { TURN_SECRET: "1234567890", TURN_HOST: "turn.example.test", TURN_TTL_SECONDS: "999999" },
    { turn: true, now: () => 1_700_000_000_000 },
  );

  assert.deepEqual(iceServers, [
    { urls: STUN_URL },
    {
      urls: [
        "turn:turn.example.test:3478?transport=udp",
        "turn:turn.example.test:3478?transport=tcp",
      ],
      username: "1700086400:bubbleplay",
      credential: "SldKiYBI923K/9zxmmhPIaexMFY=",
    },
  ]);
});

test("uses STUN only when TURN_SECRET is missing or invalid", async () => {
  assert.deepEqual(await createTurnIceServers({}, { turn: true }), [{ urls: STUN_URL }]);
  assert.deepEqual(await createTurnIceServers({ TURN_SECRET: "   " }, { turn: true }), [{ urls: STUN_URL }]);
  assert.deepEqual(await createTurnIceServers({ TURN_SECRET: 42 }, { turn: true }), [{ urls: STUN_URL }]);
});

test("TURN stays disabled until the deployment names a valid TURN_HOST", async () => {
  for (const TURN_HOST of [undefined, "", "   ", 42, "turn.example.test/path", "turn example", "turn.example.test:99999", "user@turn.example.test"]) {
    assert.deepEqual(
      await createTurnIceServers({ TURN_SECRET: "1234567890", TURN_HOST }, { turn: true }),
      [{ urls: STUN_URL }],
      String(TURN_HOST),
    );
  }
  const custom = await createTurnIceServers(
    { TURN_SECRET: "1234567890", TURN_HOST: " relay.example.test:5349 " }, { turn: true });
  assert.deepEqual(custom[1].urls, [
    "turn:relay.example.test:5349?transport=udp",
    "turn:relay.example.test:5349?transport=tcp",
  ]);
});

test("uses STUN only unless the client opted into TURN", async () => {
  for (const turn of [undefined, false, "true", 1]) {
    assert.deepEqual(await createTurnIceServers({ TURN_SECRET: "1234567890", TURN_HOST: "turn.example.test" }, { turn }), [{ urls: STUN_URL }]);
  }
});

class FakeSocket {
  constructor(expectedRole) {
    this.attachment = { role: null, expectedRole };
    this.readyState = 1;
    this.messages = [];
    this.closed = [];
  }
  deserializeAttachment() { return this.attachment; }
  serializeAttachment(value) { this.attachment = value; }
  send(message) { this.messages.push(message); }
  close(code, reason) { this.closed.push({ code, reason }); }
}

test("host relay token and preauthenticated client role remain separate", async () => {
  const storage = { values: new Map(), get: async (key) => storage.values.get(key), put: async (key, value) => storage.values.set(key, value), delete: async (key) => storage.values.delete(key) };
  const client = new FakeSocket("client");
  const host = new FakeSocket("host");
  const env = { TOKEN: "token", TURN_SECRET: "1234567890", TURN_HOST: "turn.example.test" };
  const room = new Room({ getWebSockets: () => [client, host], storage }, env);

  await room.webSocketMessage(host, JSON.stringify({ type: "hello", token: "token", role: "host" }));
  await room.webSocketMessage(client, JSON.stringify({ type: "hello", role: "client", turn: true }));

  const hostHello = JSON.parse(host.messages[0]);
  const clientHello = JSON.parse(client.messages[0]);
  assert.deepEqual(hostHello, { type: "hello-ok" });
  assert.equal(clientHello.type, "hello-ok");
  assert.equal(clientHello.iceServers.length, 2);
  const observable = JSON.stringify({ messages: [...host.messages, ...client.messages], stored: [...storage.values.values()] });
  assert.equal(observable.includes(env.TURN_SECRET), false);
  assert.equal(observable.includes("turn:turn.example.test"), true);
});

test("a client hello without the TURN opt-in never sees the TURN hostname", async () => {
  const storage = { values: new Map(), get: async (key) => storage.values.get(key), put: async (key, value) => storage.values.set(key, value), delete: async (key) => storage.values.delete(key) };
  const client = new FakeSocket("client");
  const room = new Room({ getWebSockets: () => [client], storage }, { TOKEN: "token", TURN_SECRET: "1234567890", TURN_HOST: "turn.example.test" });

  await room.webSocketMessage(client, JSON.stringify({ type: "hello", role: "client" }));

  const clientHello = JSON.parse(client.messages[0]);
  assert.equal(clientHello.type, "hello-ok");
  assert.deepEqual(clientHello.iceServers, [{ urls: STUN_URL }]);
  assert.equal(client.messages.some((message) => message.includes("turn.example.test")), false);
});

test("never authenticates a host hello when TOKEN is absent, empty, or mismatched", async () => {
  for (const env of [
    { TURN_SECRET: "1234567890", TURN_HOST: "turn.example.test" },
    { TOKEN: "", TURN_SECRET: "1234567890", TURN_HOST: "turn.example.test" },
    { TOKEN: "configured", TURN_SECRET: "1234567890", TURN_HOST: "turn.example.test" },
  ]) {
    const storage = { get: async () => [], put: async () => {}, delete: async () => {} };
    const host = new FakeSocket("host");
    const room = new Room({ getWebSockets: () => [host], storage }, env);
    await room.webSocketMessage(host, JSON.stringify({ type: "hello", token: "wrong", role: "host" }));
    assert.equal(host.messages.some((message) => JSON.parse(message).type === "hello-ok"), false);
    assert.equal(host.messages.some((message) => message.includes("turn:turn.example.test")), false);
  }
});

test("a loopback Stream Lab socket can authenticate its host without a production relay token", async () => {
  const storage = { get: async () => [], put: async () => {}, delete: async () => {} };
  const host = new FakeSocket("host");
  host.attachment.localLab = true;
  const room = new Room({ getWebSockets: () => [host], storage }, { STREAM_LAB_LOCAL_BYPASS: "1" });

  await room.webSocketMessage(host, JSON.stringify({ type: "hello", role: "host" }));

  assert.deepEqual(JSON.parse(host.messages[0]), { type: "hello-ok" });
  assert.equal(host.closed.length, 0);
});

test("a version probe reports the immutable Worker version without replacing the live host", async () => {
  const storage = { get: async () => [], put: async () => {}, delete: async () => {} };
  const host = new FakeSocket("host");
  host.attachment.role = "host";
  const probe = new FakeSocket("version-probe");
  const versionId = "2fae2311-36ac-41fa-8d8c-8607007c99cc";
  const room = new Room({ getWebSockets: () => [host, probe], storage }, {
    TOKEN: "token",
    WORKER_VERSION: { id: versionId },
  });

  await room.webSocketMessage(probe,
    JSON.stringify({ type: "hello", token: "token", role: "version-probe" }));

  assert.deepEqual(JSON.parse(probe.messages[0]), {
    type: "hello-ok",
    workerVersionId: versionId,
  });
  assert.equal(probe.closed[0].code, 1000);
  assert.deepEqual(host.closed, []);
  assert.deepEqual(host.messages, []);
});

test("the version-probe route requires host authorization before it reaches the signaling room", async () => {
  const roomNames = [];
  let forwarded = 0;
  const env = {
    ...RELAY_SECRETS,
    APPROVAL_METHODS: "host",
    GATE: {
      idFromName: () => "gate",
      get: () => ({ fetch: async () => new Response("gate") }),
    },
    ROOM: {
      idFromName: (name) => {
        roomNames.push(name);
        return name;
      },
      get: () => ({
        fetch: async () => {
          forwarded++;
          return new Response("room");
        },
      }),
    },
  };

  const rejected = await workerRuntime.fetch(new Request("https://play.example/host-version-ws"), env);
  assert.equal(rejected.status, 404);
  assert.equal(forwarded, 0);

  const accepted = await workerRuntime.fetch(new Request("https://play.example/host-version-ws", {
    headers: { Authorization: `Bearer ${RELAY_SECRETS.HOST_GATE_TOKEN}` },
  }), env);
  assert.equal(accepted.status, 200);
  assert.equal(forwarded, 1);
  assert.deepEqual(roomNames, ["room-v3"]);
});

test("a websocket entrance cannot claim the opposite role", async () => {
  const storage = { get: async () => [], put: async () => {}, delete: async () => {} };
  const socket = new FakeSocket("client");
  const room = new Room({ getWebSockets: () => [socket], storage }, { TOKEN: "host-secret" });
  await room.webSocketMessage(socket, JSON.stringify({ type: "hello", token: "host-secret", role: "host" }));
  assert.equal(socket.closed[0].code, 1008);
});
