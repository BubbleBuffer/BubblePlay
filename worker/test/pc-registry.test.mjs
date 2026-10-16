import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { ApprovalGate } from "../src/gate.js";

const CONTRACT = JSON.parse(readFileSync(new URL("../../fixtures/device-keys-contract.json", import.meta.url)));
const PC = CONTRACT.ids.find((entry) => entry.name === "pc");
const VIEWER = CONTRACT.ids.find((entry) => entry.name === "viewer");
const ALIAS = CONTRACT.ids.find((entry) => entry.name === "crockford-aliases");
const TOKEN = "registration-token";
const ADMIN = "admin-token";

class FakeStorage {
  constructor() { this.values = new Map(); }
  async get(key) { return this.values.get(key); }
  async put(key, value) { this.values.set(key, structuredClone(value)); }
  async delete(key) { this.values.delete(key); }
  async list({ prefix = "" } = {}) {
    return new Map([...this.values].filter(([key]) => key.startsWith(prefix)));
  }
}

// Behaviour per Room name: "online", "offline" (the default), "throw", or
// "delete-fail" (online, but the delete call answers 500). The object is read
// on every call, so a test can change a Room's state mid-flow.
function roomNamespace(behaviour, calls) {
  return {
    idFromName: (name) => name,
    get: (name) => ({
      async fetch(request) {
        const path = new URL(request.url).pathname;
        calls.push(`${request.method} ${name} ${path}`);
        const mode = behaviour[name] ?? "offline";
        if (mode === "throw") throw new Error("room unavailable");
        if (path === "/__pc/delete") return new Response(null, { status: mode === "delete-fail" ? 500 : 204 });
        return new Response(JSON.stringify({ online: mode === "online" }), { headers: { "Content-Type": "application/json" } });
      },
    }),
  };
}

function setup(t, { rooms = {} } = {}) {
  const clock = { now: 1_000 };
  t.mock.method(Date, "now", () => clock.now);
  const storage = new FakeStorage();
  const roomCalls = [];
  const env = {
    TOKEN: TOKEN,
    GATE_ADMIN_TOKEN: ADMIN,
    NTFY_URL: "https://ntfy.example.test",
    NTFY_TOPIC: "private-topic",
    NTFY_TOKEN: "publisher-token",
    APPROVAL_METHODS: "ntfy",
    ROOM: roomNamespace(rooms, roomCalls),
  };
  const gate = new ApprovalGate({ storage }, env);
  return { gate, storage, env, clock, rooms, roomCalls };
}

function internal(gate, path, { method = "POST", body, host = "gate.internal", headers = {} } = {}) {
  const init = { method, headers: { ...headers } };
  if (body !== undefined) {
    init.body = JSON.stringify(body);
    init.headers["Content-Type"] = "application/json";
  }
  return gate.fetch(new Request(`https://${host}${path}`, init));
}

function admin(gate, path, { method = "GET" } = {}) {
  return internal(gate, path, { method, host: "gate.internal", headers: { Authorization: `Bearer ${ADMIN}` } });
}

const registration = (pc, extra = {}) => ({ id: pc.id, public: pc.public, ...extra });
const pcKeys = (storage) => [...storage.values.keys()].filter((key) => key.startsWith("pc:"));

test("registration needs the TOKEN for an unknown PC and writes nothing without it", async (t) => {
  const { gate, storage } = setup(t);
  assert.equal((await internal(gate, "/__pc/lookup", { body: registration(PC) })).status, 404);
  assert.equal((await internal(gate, "/__pc/register", { body: registration(PC) })).status, 404);
  assert.equal((await internal(gate, "/__pc/register", { body: registration(PC, { token: "" }) })).status, 404);
  assert.deepEqual(pcKeys(storage), []);
});

test("a valid token registers an unknown PC; lookup alone does not write", async (t) => {
  const { gate, storage, clock } = setup(t);
  assert.equal((await internal(gate, "/__pc/lookup", { body: registration(PC, { token: TOKEN }) })).status, 204);
  assert.deepEqual(pcKeys(storage), [], "lookup must not write");
  clock.now = 2_000;
  assert.equal((await internal(gate, "/__pc/register", { body: registration(PC, { token: TOKEN }) })).status, 204);
  assert.deepEqual(storage.values.get(`pc:${PC.id}`), { public: PC.public, firstSeen: 2_000, lastSeen: 2_000 });
});

test("a wrong token is refused for an unknown PC, and a known PC with a wrong token is refused too", async (t) => {
  const { gate, storage, clock } = setup(t);
  for (const token of ["wrong", " registration-token", 12345, ""]) {
    assert.equal((await internal(gate, "/__pc/register", { body: registration(PC, { token }) })).status, 404, JSON.stringify(token));
  }
  clock.now = 3_000;
  assert.equal((await internal(gate, "/__pc/register", { body: registration(PC, { token: TOKEN }) })).status, 204);
  const before = structuredClone(storage.values.get(`pc:${PC.id}`));
  clock.now = 4_000;
  assert.equal((await internal(gate, "/__pc/lookup", { body: registration(PC, { token: "wrong" }) })).status, 404);
  assert.equal((await internal(gate, "/__pc/register", { body: registration(PC, { token: "wrong" }) })).status, 404);
  assert.deepEqual(storage.values.get(`pc:${PC.id}`), before, "refused call must not touch the record");
});

test("a known PC with the same public needs no token; firstSeen is kept and lastSeen moves on register only", async (t) => {
  const { gate, storage, clock } = setup(t);
  clock.now = 1_000;
  await internal(gate, "/__pc/register", { body: registration(PC, { token: TOKEN }) });
  clock.now = 5_000;
  assert.equal((await internal(gate, "/__pc/register", { body: registration(PC) })).status, 204);
  assert.deepEqual(storage.values.get(`pc:${PC.id}`), { public: PC.public, firstSeen: 1_000, lastSeen: 5_000 });
  clock.now = 9_000;
  assert.equal((await internal(gate, "/__pc/lookup", { body: registration(PC) })).status, 204);
  assert.deepEqual(storage.values.get(`pc:${PC.id}`), { public: PC.public, firstSeen: 1_000, lastSeen: 5_000 });
});

test("a different public key never matches a registered ID", async (t) => {
  const { gate, storage } = setup(t);
  await internal(gate, "/__pc/register", { body: registration(PC, { token: TOKEN }) });
  const body = { id: PC.id, public: VIEWER.public };
  assert.equal((await internal(gate, "/__pc/lookup", { body })).status, 404);
  assert.equal((await internal(gate, "/__pc/register", { body: { ...body, token: TOKEN } })).status, 404);
  assert.equal(storage.values.get(`pc:${PC.id}`).public, PC.public);
});

test("the ID must be derived from the public key; aliases are stored under the canonical ID", async (t) => {
  const { gate, storage } = setup(t);
  assert.equal((await internal(gate, "/__pc/register", { body: { id: VIEWER.id, public: PC.public, token: TOKEN } })).status, 404);
  assert.equal((await internal(gate, "/__pc/register", { body: { id: "not-an-id", public: ALIAS.public, token: TOKEN } })).status, 404);
  assert.equal((await internal(gate, "/__pc/register", { body: { id: ALIAS.parse.input, public: ALIAS.public, token: TOKEN } })).status, 204);
  assert.deepEqual(pcKeys(storage), [`pc:${ALIAS.id}`]);
  assert.equal((await internal(gate, "/__pc/lookup", { body: { id: ALIAS.id.replaceAll("-", "").toLowerCase(), public: ALIAS.public } })).status, 204);
  assert.equal((await internal(gate, "/__pc/register", { body: { id: PC.id, public: PC.public } })).status, 404, "no token for an unknown ID");
  assert.equal((await internal(gate, "/__pc/register", { body: { public: PC.public, token: TOKEN } })).status, 404, "missing ID");
  assert.equal((await internal(gate, "/__pc/register", { body: null })).status, 404, "body must be an object");
  assert.equal((await internal(gate, "/__pc/register", { body: { id: PC.id, public: PC.public.slice(0, 42), token: TOKEN } })).status, 404, "malformed public key");
});

test("revocation during the challenge refuses a register that lookup had already accepted", async (t) => {
  const { gate, storage } = setup(t);
  await internal(gate, "/__pc/register", { body: registration(PC, { token: TOKEN }) });
  assert.equal((await internal(gate, "/__pc/lookup", { body: registration(PC) })).status, 204, "lookup accepted before revocation");
  assert.equal((await admin(gate, `/__gate/manage/pcs/${PC.id}`, { method: "DELETE" })).status, 204);
  assert.equal((await internal(gate, "/__pc/register", { body: registration(PC) })).status, 404, "register must re-check and refuse");
  assert.deepEqual(pcKeys(storage), []);
});

test("register and admin delete for the same PC are serialized, so the registry ends empty", async (t) => {
  const { gate, storage } = setup(t);
  await internal(gate, "/__pc/register", { body: registration(PC, { token: TOKEN }) });
  const [registered, deleted] = await Promise.all([
    internal(gate, "/__pc/register", { body: registration(PC) }),
    admin(gate, `/__gate/manage/pcs/${PC.id}`, { method: "DELETE" }),
  ]);
  assert.equal(deleted.status, 204);
  assert.ok([204, 404].includes(registered.status));
  assert.deepEqual(pcKeys(storage), [], "a register that lost the race must not recreate the row");
});

test("admin list requires the admin bearer and reports online state live, failing closed", async (t) => {
  const { gate, roomCalls } = setup(t, { rooms: { [`pc-v1:${PC.id}`]: "online", [`pc-v1:${VIEWER.id}`]: "throw" } });
  await internal(gate, "/__pc/register", { body: registration(PC, { token: TOKEN }) });
  await internal(gate, "/__pc/register", { body: registration({ id: VIEWER.id, public: VIEWER.public }, { token: TOKEN }) });
  assert.equal((await internal(gate, "/__gate/manage/pcs")).status, 404, "no bearer");
  assert.equal((await internal(gate, "/__gate/manage/pcs", { method: "GET", headers: { Authorization: "Bearer nope" } })).status, 404);
  const response = await admin(gate, "/__gate/manage/pcs");
  assert.equal(response.status, 200);
  const rows = await response.json();
  assert.deepEqual(rows.map((row) => row.id), [PC.id, VIEWER.id].sort());
  const online = Object.fromEntries(rows.map((row) => [row.id, row.online]));
  assert.equal(online[PC.id], true);
  assert.equal(online[VIEWER.id], false, "a Room error counts as offline");
  assert.deepEqual(Object.keys(rows[0]).sort(), ["firstSeen", "id", "lastSeen", "online", "public"]);
  assert.ok(roomCalls.includes(`GET pc-v1:${PC.id} /__pc/online`), "online is asked live from the PC's Room");
});

test("admin delete accepts aliases, removes the row, and asks the PC's Room to delete", async (t) => {
  const { gate, storage, roomCalls } = setup(t);
  await internal(gate, "/__pc/register", { body: registration(PC, { token: TOKEN }) });
  const alias = PC.id.replaceAll("-", "").toLowerCase();
  assert.equal((await gate.fetch(new Request(`https://gate.internal/__gate/manage/pcs/${alias}`, {
    method: "DELETE", headers: { Authorization: "Bearer wrong" },
  }))).status, 404, "bad bearer");
  assert.equal(storage.values.has(`pc:${PC.id}`), true, "bad bearer deletes nothing");
  assert.equal((await admin(gate, `/__gate/manage/pcs/${alias}`, { method: "DELETE" })).status, 204);
  assert.deepEqual(pcKeys(storage), []);
  assert.ok(roomCalls.includes(`POST pc-v1:${PC.id} /__pc/delete`));
  assert.equal((await admin(gate, "/__gate/manage/pcs/not-a-real-id", { method: "DELETE" })).status, 404, "bad ID");
  assert.equal((await admin(gate, `/__gate/manage/pcs/${PC.id}`, { method: "POST" })).status, 404, "wrong method");
});

test("a failed Room delete returns 503 after the row is gone, and a retry completes", async (t) => {
  const rooms = { [`pc-v1:${PC.id}`]: "delete-fail" };
  const { gate, storage } = setup(t, { rooms });
  await internal(gate, "/__pc/register", { body: registration(PC, { token: TOKEN }) });
  assert.equal((await admin(gate, `/__gate/manage/pcs/${PC.id}`, { method: "DELETE" })).status, 503);
  assert.deepEqual(pcKeys(storage), [], "the registry change is not rolled back");
  assert.equal((await internal(gate, "/__pc/register", { body: registration(PC) })).status, 404, "a revoked PC is refused");
  rooms[`pc-v1:${PC.id}`] = "online";
  assert.equal((await admin(gate, `/__gate/manage/pcs/${PC.id}`, { method: "DELETE" })).status, 204);
});

test("resolve picks the newest online PC, then legacy when online, then the newest listed PC, then legacy", async (t) => {
  const rooms = {};
  const { gate, storage, clock, roomCalls } = setup(t, { rooms });
  const resolve = async () => (await (await internal(gate, "/__pc/resolve", { method: "GET" })).json()).pc;

  assert.equal(await resolve(), null, "nothing listed, legacy offline");
  rooms["room-v3"] = "online";
  assert.equal(await resolve(), null, "nothing listed, legacy online");
  rooms["room-v3"] = "offline";

  clock.now = 1_000;
  await internal(gate, "/__pc/register", { body: registration(PC, { token: TOKEN }) });
  assert.equal(await resolve(), PC.id, "one listed, offline: the sole listed PC");

  clock.now = 2_000;
  await internal(gate, "/__pc/register", { body: registration(VIEWER, { token: TOKEN }) });
  assert.equal(await resolve(), VIEWER.id, "two offline: the newest listed PC");

  rooms[`pc-v1:${PC.id}`] = "online";
  assert.equal(await resolve(), PC.id, "an online PC beats an offline newer one");

  rooms[`pc-v1:${VIEWER.id}`] = "online";
  assert.equal(await resolve(), VIEWER.id, "two online: the newest by lastSeen");

  rooms["room-v3"] = "online";
  roomCalls.length = 0;
  assert.equal(await resolve(), VIEWER.id, "online PC beats online legacy");
  assert.ok(!roomCalls.some((call) => call.includes("room-v3")), "legacy is not asked when a PC is online");

  rooms[`pc-v1:${PC.id}`] = "offline";
  rooms[`pc-v1:${VIEWER.id}`] = "offline";
  assert.equal(await resolve(), null, "no PC online and legacy online: legacy");

  rooms["room-v3"] = "throw";
  assert.equal(await resolve(), VIEWER.id, "a failing legacy check is offline");
  rooms["room-v3"] = "offline";

  rooms[`pc-v1:${PC.id}`] = "online";
  rooms[`pc-v1:${VIEWER.id}`] = "throw";
  assert.equal(await resolve(), PC.id, "a failing check counts as offline, so the online older PC wins");
  assert.equal(pcKeys(storage).length, 2, "resolve writes nothing");
});

test("resolve breaks an equal lastSeen tie by ID, deterministically", async (t) => {
  const rooms = { [`pc-v1:${PC.id}`]: "online", [`pc-v1:${VIEWER.id}`]: "online" };
  const { gate, clock } = setup(t, { rooms });
  clock.now = 7_000;
  await internal(gate, "/__pc/register", { body: registration(PC, { token: TOKEN }) });
  await internal(gate, "/__pc/register", { body: registration(VIEWER, { token: TOKEN }) });
  const first = await (await internal(gate, "/__pc/resolve", { method: "GET" })).json();
  const second = await (await internal(gate, "/__pc/resolve", { method: "GET" })).json();
  assert.deepEqual(first, second);
  assert.equal(first.pc, [PC.id, VIEWER.id].sort()[0]);
});

test("presence is a no-op: it answers 204 and never creates, touches or reads a PC row", async (t) => {
  const { gate, storage, clock } = setup(t);
  await internal(gate, "/__pc/register", { body: registration(PC, { token: TOKEN }) });
  const before = structuredClone([...storage.values]);
  clock.now = 9_000;
  assert.equal((await internal(gate, "/__pc/presence", { body: { id: PC.id, online: true } })).status, 204);
  assert.equal((await internal(gate, "/__pc/presence", { body: { id: PC.id, online: false } })).status, 204);
  assert.equal((await internal(gate, "/__pc/presence", { body: { id: null, online: false } })).status, 204);
  assert.equal((await internal(gate, "/__pc/presence", { body: { id: VIEWER.id, online: true } })).status, 204);
  assert.deepEqual([...storage.values], before, "presence writes nothing");
  assert.equal((await internal(gate, "/__pc/presence", { body: { id: PC.id, online: "yes" } })).status, 404);
  assert.equal((await internal(gate, "/__pc/presence", { body: { id: PC.id.toLowerCase(), online: true } })).status, 404);
  assert.equal((await internal(gate, "/__pc/presence", { body: { id: "x", online: true } })).status, 404);
});

test("the /__pc paths answer only on gate.internal; public hosts get 404 and never enter the approval hold", async (t) => {
  const { gate, storage, env, roomCalls } = setup(t);
  const notices = [];
  env.__gateFetch = async (_url, init) => { notices.push(init); return new Response(null, { status: 200 }); };
  const body = registration(PC, { token: TOKEN });
  for (const host of ["playfull-cat.example", "gate.example", "room.internal"]) {
    assert.equal((await internal(gate, "/__pc/resolve", { method: "GET", host })).status, 404, host);
    assert.equal((await internal(gate, "/__pc/lookup", { body, host })).status, 404, host);
    assert.equal((await internal(gate, "/__pc/register", { body, host })).status, 404, host);
    assert.equal((await internal(gate, "/__pc/presence", { body: { id: null, online: true }, host })).status, 404, host);
  }
  assert.equal(gate.pendingById.size, 0, "no approval request was held");
  assert.deepEqual(notices, [], "no ntfy notice was sent");
  assert.deepEqual(pcKeys(storage), [], "nothing was registered from the public host");
  assert.deepEqual(roomCalls, []);
});

test("unknown /__pc paths and wrong methods are hidden even on gate.internal", async (t) => {
  const { gate } = setup(t);
  assert.equal((await internal(gate, "/__pc/nope", { body: {} })).status, 404);
  assert.equal((await internal(gate, "/__pc/resolve", { method: "POST", body: {} })).status, 404);
  assert.equal((await internal(gate, "/__pc/lookup", { method: "GET" })).status, 404);
});

test("any present token key must be the correct string for known and unknown PCs, including null and false", async (t) => {
  const { gate, storage, clock } = setup(t);
  clock.now = 3_000;
  await internal(gate, "/__pc/register", { body: registration(PC, { token: TOKEN }) });
  const before = structuredClone(storage.values.get(`pc:${PC.id}`));
  clock.now = 4_000;
  for (const token of [null, false, 0, 1, {}, [], TOKEN.toUpperCase(), `${TOKEN} `, ""]) {
    for (const path of ["/__pc/lookup", "/__pc/register"]) {
      assert.equal((await internal(gate, path, { body: registration(PC, { token }) })).status, 404, `${path} ${JSON.stringify(token)}`);
    }
  }
  assert.deepEqual(storage.values.get(`pc:${PC.id}`), before, "refused calls must not write");
  const unknown = { id: VIEWER.id, public: VIEWER.public };
  for (const token of [null, false, ""]) {
    assert.equal((await internal(gate, "/__pc/register", { body: { ...unknown, token } })).status, 404, `unknown ${JSON.stringify(token)}`);
  }
  assert.equal((await internal(gate, "/__pc/register", { body: { ...unknown, token: TOKEN } })).status, 204, "the correct token still works");
});

test("an omitted token key is not a token: known PCs are admitted, unknown PCs are refused", async (t) => {
  const { gate, storage } = setup(t);
  await internal(gate, "/__pc/register", { body: registration(PC, { token: TOKEN }) });
  assert.equal((await internal(gate, "/__pc/lookup", { body: { id: PC.id, public: PC.public } })).status, 204);
  assert.equal((await internal(gate, "/__pc/register", { body: { id: VIEWER.id, public: VIEWER.public } })).status, 404);
  assert.deepEqual(pcKeys(storage), [`pc:${PC.id}`]);
});

test("internal PC routes accept exactly https://gate.internal and nothing near it", async (t) => {
  const { gate, storage } = setup(t);
  const body = JSON.stringify(registration(PC, { token: TOKEN }));
  const send = (url, method = "POST") => gate.fetch(new Request(url, {
    method, headers: { "Content-Type": "application/json" }, body: method === "POST" ? body : undefined,
  }));
  for (const url of [
    "http://gate.internal/__pc/register",
    "http://gate.internal/__pc/lookup",
    "https://gate.internal:8443/__pc/register",
    "http://gate.internal:80/__pc/register",
    "https://gate.internal.example/__pc/register",
    "https://evil.example/__pc/register",
  ]) assert.equal((await send(url)).status, 404, url);
  assert.equal((await send("http://gate.internal/__pc/resolve", "GET")).status, 404);
  assert.equal((await send("https://gate.internal:8443/__pc/resolve", "GET")).status, 404);
  assert.equal((await send("https://gate.internal:8443/__pc/presence")).status, 404);
  assert.equal(gate.pendingById.size, 0, "refused origins never enter the approval hold");
  assert.deepEqual(pcKeys(storage), [], "nothing was registered from a foreign origin");
  assert.equal((await send("https://gate.internal/__pc/register")).status, 204, "the exact origin works");
  assert.equal((await send("https://gate.internal/__pc/resolve", "GET")).status, 200);
});
