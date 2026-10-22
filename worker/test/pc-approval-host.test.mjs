import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { ApprovalGate } from "../src/gate.js";

const CONTRACT = JSON.parse(readFileSync(new URL("../../fixtures/device-keys-contract.json", import.meta.url)));
const APPROVAL_CONTRACT = JSON.parse(readFileSync(new URL("../../fixtures/browser-approval-contract.json", import.meta.url)));
const PC = CONTRACT.ids.find((entry) => entry.name === "pc");
const VIEWER = CONTRACT.ids.find((entry) => entry.name === "viewer");
const LEGACY = "room-v3";
const pcRoom = (entry) => `pc-v1:${entry.id}`;

class FakeStorage {
  constructor() { this.values = new Map(); }
  async get(key) { return this.values.get(key); }
  async put(key, value) { this.values.set(key, structuredClone(value)); }
  async delete(key) { this.values.delete(key); }
  async list({ prefix = "" } = {}) {
    return new Map([...this.values].filter(([key]) => key.startsWith(prefix)));
  }
}

// A minimal Room: hostOnline comes from the `online` set, and every request
// and cancel is recorded with the Room name it was sent to. `throwing` lists
// Rooms whose status check fails.
function fixture({ online = [], throwing = [], registered = [] } = {}) {
  const storage = new FakeStorage();
  const online_ = new Set(online);
  const seen = { requests: [], cancels: [], statusCalls: [] };
  const env = {
    APPROVAL_METHODS: "host",
    GATE_ADMIN_TOKEN: "admin-token",
    TOKEN: "registration-token",
    ASSETS: { async fetch() { return new Response('<body class="auth-page">Sign in</body>', { headers: { "Content-Type": "text/html" } }); } },
    ROOM: {
      idFromName: (name) => name,
      get: (name) => ({
        async fetch(request) {
          const path = new URL(request.url).pathname;
          if (path === "/__approval/status") {
            seen.statusCalls.push(name);
            if (throwing.includes(name)) throw new Error("room unavailable");
            return Response.json({ hostOnline: online_.has(name) });
          }
          if (path === "/__approval/request") {
            seen.requests.push({ room: name, body: await request.json() });
            return new Response(null, { status: online_.has(name) ? 204 : 503 });
          }
          if (path === "/__approval/cancel") {
            seen.cancels.push({ room: name, body: await request.json() });
            return new Response(null, { status: 204 });
          }
          return new Response(null, { status: 404 });
        },
      }),
    },
  };
  for (const entry of registered) {
    storage.values.set(`pc:${entry.pc.id}`, { public: entry.pc.public, firstSeen: 1, lastSeen: entry.lastSeen });
  }
  const gate = new ApprovalGate({ storage }, env);
  return { gate, storage, env, seen, online: online_ };
}

function navigation() {
  return new Request("https://playfull-cat.example/", { headers: { Accept: "text/html", "CF-Connecting-IP": "203.0.113.7" } });
}

function hostDecision(id, approve) {
  return new Request("https://gate.internal/__approval/decision", {
    method: "POST", headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ ...APPROVAL_CONTRACT.decision, id, approve }),
  });
}

async function until(predicate) {
  for (let attempt = 0; attempt < 500; attempt += 1) {
    if (predicate()) return;
    await new Promise((resolve) => setTimeout(resolve, 2));
  }
  throw new Error("condition not reached");
}

const distinct = (values) => [...new Set(values)];

test("the legacy room is chosen first, and PCs are not asked while it is online", async () => {
  const f = fixture({ online: [LEGACY, pcRoom(PC)], registered: [{ pc: PC, lastSeen: 1_000 }] });
  const held = f.gate.fetch(navigation());
  await until(() => f.seen.requests.length === 1);
  assert.equal(f.seen.requests[0].room, LEGACY);
  assert.deepEqual(distinct(f.seen.statusCalls), [LEGACY]);
  await f.gate.fetch(hostDecision(f.seen.requests[0].body.id, false));
  assert.equal((await held).status, 404);
});

test("with the legacy room offline, the newest online PC by lastSeen gets the request and older PCs are not asked", async () => {
  const f = fixture({
    online: [pcRoom(PC), pcRoom(VIEWER)],
    registered: [{ pc: PC, lastSeen: 1_000 }, { pc: VIEWER, lastSeen: 5_000 }],
  });
  const held = f.gate.fetch(navigation());
  await until(() => f.seen.requests.length === 1);
  assert.equal(f.seen.requests[0].room, pcRoom(VIEWER));
  assert.deepEqual(distinct(f.seen.statusCalls), [LEGACY, pcRoom(VIEWER)], "the search stops at the first online host");
  await f.gate.fetch(hostDecision(f.seen.requests[0].body.id, false));
  assert.equal((await held).status, 404);
});

test("offline PCs are skipped, and the next listed PC by lastSeen is chosen", async () => {
  const f = fixture({
    online: [pcRoom(PC)],
    registered: [{ pc: PC, lastSeen: 1_000 }, { pc: VIEWER, lastSeen: 5_000 }],
  });
  const held = f.gate.fetch(navigation());
  await until(() => f.seen.requests.length === 1);
  assert.equal(f.seen.requests[0].room, pcRoom(PC));
  assert.deepEqual(distinct(f.seen.statusCalls), [LEGACY, pcRoom(VIEWER), pcRoom(PC)]);
  await f.gate.fetch(hostDecision(f.seen.requests[0].body.id, false));
  assert.equal((await held).status, 404);
});

test("a failing legacy status check counts as offline and the PC is chosen", async () => {
  const f = fixture({ online: [pcRoom(PC)], throwing: [LEGACY], registered: [{ pc: PC, lastSeen: 1_000 }] });
  const held = f.gate.fetch(navigation());
  await until(() => f.seen.requests.length === 1);
  assert.equal(f.seen.requests[0].room, pcRoom(PC));
  await f.gate.fetch(hostDecision(f.seen.requests[0].body.id, false));
  assert.equal((await held).status, 404);
});

test("with no capable host, the approval-required page is served and nothing is published", async () => {
  const f = fixture({ online: [], registered: [{ pc: PC, lastSeen: 1_000 }] });
  const response = await f.gate.fetch(navigation());
  assert.equal(response.status, 503);
  assert.match(await response.text(), /data-approval-required/);
  assert.deepEqual(f.seen.requests, []);
  assert.equal(f.gate.pendingById.size, 0);
});

test("/__auth/methods reports hostOnline when only a registered PC host is online", async () => {
  const f = fixture({ online: [pcRoom(PC)], registered: [{ pc: PC, lastSeen: 1_000 }] });
  const response = await f.gate.fetch(new Request("https://gate.internal/__auth/methods"));
  assert.equal((await response.json()).hostOnline, true);
});

test("cancellation goes to the legacy room that received the request, even after the default host moves to a PC", async () => {
  const f = fixture({ online: [LEGACY], registered: [{ pc: PC, lastSeen: 1_000 }] });
  const held = f.gate.fetch(navigation());
  await until(() => f.seen.requests.length === 1);
  const id = f.seen.requests[0].body.id;
  assert.equal(f.gate.pendingById.get(id).hostRoomName, LEGACY, "the room is pinned on the pending request");

  f.online.delete(LEGACY);
  f.online.add(pcRoom(PC));
  assert.equal(await f.gate.hostOnline(), true, "the default has moved to the PC");

  assert.equal((await f.gate.fetch(hostDecision(id, false))).status, 204);
  assert.equal((await held).status, 404);
  await until(() => f.seen.cancels.length === 1);
  assert.deepEqual(f.seen.cancels.map((cancel) => [cancel.room, cancel.body.id]), [[LEGACY, id]]);
});

test("cancellation goes to the PC that received the request, even after the PC goes offline and the legacy room comes back", async () => {
  const f = fixture({ online: [pcRoom(PC)], registered: [{ pc: PC, lastSeen: 1_000 }] });
  const held = f.gate.fetch(navigation());
  await until(() => f.seen.requests.length === 1);
  const id = f.seen.requests[0].body.id;
  assert.equal(f.seen.requests[0].room, pcRoom(PC));
  assert.equal(f.gate.pendingById.get(id).hostRoomName, pcRoom(PC));

  f.online.delete(pcRoom(PC));
  f.online.add(LEGACY);

  assert.equal((await f.gate.fetch(hostDecision(id, false))).status, 204);
  assert.equal((await held).status, 404);
  await until(() => f.seen.cancels.length === 1);
  assert.deepEqual(f.seen.cancels.map((cancel) => [cancel.room, cancel.body.id]), [[pcRoom(PC), id]]);
});
