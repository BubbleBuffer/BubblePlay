import test from "node:test";
import assert from "node:assert/strict";
import { ApprovalGate, isInitialNavigation } from "../src/gate.js";
import { approvalMethods } from "../src/approval-methods.js";
import { readFileSync } from "node:fs";
import worker, { weakSecretNames } from "../src/index.js";
import { RELAY_SECRETS } from "./helpers/relay-secrets.mjs";
const WORKER_SECRETS = { ...RELAY_SECRETS, APPROVAL_METHODS: "host" };
const APPROVAL_CONTRACT = JSON.parse(readFileSync(new URL("../../fixtures/browser-approval-contract.json", import.meta.url)));

class FakeStorage {
  constructor() { this.values = new Map(); }
  async get(key) { return this.values.get(key); }
  async put(key, value) { this.values.set(key, structuredClone(value)); }
  async delete(key) { this.values.delete(key); }
  async list({ prefix = "" } = {}) {
    return new Map([...this.values].filter(([key]) => key.startsWith(prefix)));
  }
}

function fixture() {
  const notices = [];
  const assets = [];
  const storage = new FakeStorage();
  const env = {
    ASSETS: {
      async fetch(request) {
        const path = new URL(request.url).pathname;
        assets.push(path);
        return new Response(path === "/login.html" ? "Sign in" : "BubblePlay", { headers: { "Content-Type": "text/html" } });
      },
    },
    NTFY_URL: "https://ntfy.example.test",
    NTFY_TOPIC: "private-topic",
    NTFY_TOKEN: "publisher-token",
    GATE_ADMIN_TOKEN: "admin-token",
    APP_PASSWORD: "correct horse battery staple",
    GATE_APPROVAL_TTL_SECONDS: "60",
    __gateFetch: async (_url, init) => {
      notices.push({ ...init, body: JSON.parse(init.body) });
      return new Response(null, { status: 200 });
    },
  };
  const gate = new ApprovalGate({ storage }, env);
  return { gate, env, storage, notices, assets };
}

async function approveNavigation(f, ip = "203.0.113.7") {
  const previousNotices = f.notices.length;
  const held = f.gate.fetch(navigation(ip));
  await waitFor(() => f.notices.length > previousNotices);
  const approve = f.notices.at(-1).body.actions.find((action) => action.label === "Approve");
  await f.gate.fetch(decisionRequest(approve));
  return held;
}

function navigation(ip = "203.0.113.7") {
  return new Request("https://playfull-cat.example/", {
    headers: {
      Accept: "text/html,application/xhtml+xml",
      "CF-Connecting-IP": ip,
      "Sec-Fetch-Mode": "navigate",
      "Sec-Fetch-Dest": "document",
      "User-Agent": "Test Browser",
    },
  });
}

function decisionRequest(action) {
  return new Request("https://playfull-cat.example/__gate/decision", {
    method: "POST",
    headers: { Authorization: `Bearer ${action.headers.Authorization.slice(7)}` },
    body: action.body,
  });
}

function loginRequest(gateCookie, password, remember = true, ip = "203.0.113.7") {
  return new Request("https://playfull-cat.example/__auth/login", {
    method: "POST",
    headers: { Cookie: gateCookie, Origin: "https://playfull-cat.example", "Content-Type": "application/json", "CF-Connecting-IP": ip },
    body: JSON.stringify({ password, remember }),
  });
}

async function authenticatedBrowser(f, remember = true) {
  const approved = await approveNavigation(f);
  const gateCookie = approved.headers.get("Set-Cookie").split(";", 1)[0];
  const login = await f.gate.fetch(loginRequest(gateCookie, "correct horse battery staple", remember));
  const appCookie = login.headers.get("Set-Cookie").match(/__Host-bp-session=[^;,]+/)?.[0];
  assert.ok(appCookie);
  return { gateCookie, appCookie, cookie: `${gateCookie}; ${appCookie}` };
}

const DEBUG_ATTEMPT = "123e4567-e89b-42d3-a456-426614174000";
const DEBUG_RUN = "cab5bd95-81a3-4ef6-9fc6-8566abbb17d5";
const DEBUG_COMPATIBILITY = "wss-v4-bench2-hevc-tiles-bpip4-v16";

function diagnosticRequest(cookie, payload, origin = "https://playfull-cat.example") {
  return new Request("https://playfull-cat.example/__diag/client", {
    method: "POST",
    headers: { Cookie: cookie, Origin: origin, "Content-Type": "application/json" },
    body: JSON.stringify(payload),
  });
}

function diagnosticEvent(seq, stage = "bootstrap-inline") {
  return {
    schemaVersion: 1,
    attemptId: DEBUG_ATTEMPT,
    runId: DEBUG_RUN,
    compatibilityId: DEBUG_COMPATIBILITY,
    receiverBuild: null,
    seq,
    atMs: 1_788_192_520_304 + seq,
    stage,
    detail: { transport: "wss", pathname: "/" },
  };
}

async function waitFor(predicate, timeoutMs = 10_000, schedule = setTimeout) {
  const deadline = Date.now() + timeoutMs;
  while (!predicate()) {
    if (Date.now() >= deadline) throw new Error("condition did not become true");
    await new Promise((resolve) => schedule(resolve, 5));
  }
}

test("recognizes only the configured initial document navigation", () => {
  assert.equal(isInitialNavigation(navigation()), true);
  assert.equal(isInitialNavigation(new Request("https://x.test/app.js", { headers: { Accept: "text/html" } })), false);
  assert.equal(isInitialNavigation(new Request("https://x.test/", { method: "POST", body: "x" })), false);
  assert.equal(isInitialNavigation(new Request("https://x.test/", { headers: { Accept: "text/html", "Sec-Fetch-Dest": "image" } })), false);
});

function hostNotifier(f, online = true) {
  const messages = [];
  f.env.ROOM = { idFromName: name => name, get: name => ({ async fetch(request) {
    assert.equal(name, "room-v3");
    const path = new URL(request.url).pathname;
    if (path === "/__approval/status") return Response.json({ hostOnline: online });
    messages.push(await request.json());
    return new Response(null, { status: online ? 204 : 503 });
  } }) };
  return messages;
}

function hostDecision(id, approve) {
  return new Request("https://gate.internal/__approval/decision", { method: "POST",
    headers: { "Content-Type": "application/json" }, body: JSON.stringify({ ...APPROVAL_CONTRACT.decision, id, approve }) });
}

function socketRequest(cookie) {
  return new Request("https://gate.internal/__auth/socket", { headers: { Cookie: cookie } });
}

function internalPost(path, body) {
  return new Request(`https://gate.internal${path}`, { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) });
}

test("Cloudflare method configuration defaults to ntfy and rejects invalid or unconfigured methods", async () => {
  const f = fixture();
  assert.deepEqual(approvalMethods(f.env), ["ntfy"]);
  for (const value of ["", " ", "totp", "host,", "ntfy,bogus", null]) {
    f.env.APPROVAL_METHODS = value;
    assert.throws(() => approvalMethods(f.env));
    assert.equal((await f.gate.fetch(new Request("https://playfull-cat.example/__auth/methods"))).status, 503);
  }
  f.env.APPROVAL_METHODS = "host";
  delete f.env.NTFY_TOKEN;
  assert.deepEqual(approvalMethods(f.env), ["host"]);
  f.env.APPROVAL_METHODS = "ntfy,host";
  assert.throws(() => approvalMethods(f.env));
});

test("host-only discovery and offline navigation fail promptly without a password form or pending slot", async () => {
  const f = fixture();
  f.env.APPROVAL_METHODS = "host";
  f.env.ASSETS.fetch = async () => new Response('<body class="auth-page">Sign in</body>');
  const messages = hostNotifier(f, false);
  assert.deepEqual(await (await f.gate.fetch(new Request("https://playfull-cat.example/__auth/methods"))).json(),
    { totp: false, approval: ["host"], hostOnline: false, sourceURL: "https://github.com/BubbleBuffer/BubblePlay" });
  const response = await f.gate.fetch(navigation());
  assert.equal(response.status, 503);
  assert.match(await response.text(), /data-approval-required/);
  assert.equal(f.gate.pendingById.size, 0);
  assert.equal(messages.length, 0);
  assert.equal(f.notices.length, 0);
});

test("host requests follow the fixture, approve once and cancel after completion", async () => {
  const f = fixture();
  f.env.APPROVAL_METHODS = "host";
  const messages = hostNotifier(f);
  const held = f.gate.fetch(navigation());
  await waitFor(() => messages.length === 1);
  const request = messages[0];
  assert.deepEqual(Object.keys(request).sort(), Object.keys(APPROVAL_CONTRACT.request).sort());
  assert.match(request.id, /^[A-Za-z0-9_-]{22,64}$/);
  assert.equal(request.client, APPROVAL_CONTRACT.request.client);
  assert.ok(request.agent.length <= APPROVAL_CONTRACT.limits.agentMax);
  assert.ok(request.expiresAt > Date.now());
  for (const invalid of [...APPROVAL_CONTRACT.invalid.filter(message => message.type === "approval-decision"),
    { type: "approval-decision", id: request.id, approve: true, extra: true },
    { type: "approval-cancel", id: request.id }]) {
    assert.equal((await f.gate.fetch(internalPost("/__approval/decision", invalid))).status, 404);
    assert.equal(f.gate.pendingById.size, 1);
  }
  await f.gate.fetch(hostDecision(request.id, true));
  assert.equal((await held).status, 200);
  await waitFor(() => messages.length === 2);
  assert.deepEqual(messages[1], { ...APPROVAL_CONTRACT.cancel, id: request.id });
  await f.gate.fetch(hostDecision(request.id, false));
  assert.equal(f.assets.length, 1);
});

test("host approval masks actual IPv6 network bits before formatting compressed prefixes", async () => {
  for (const [ip, expected] of [
    ["2001:db8::1234", "2001:db8::/64"],
    ["::1", "::/64"],
    ["2001:0DB8:1234:5678:9abc:def0:1111:2222", "2001:db8:1234:5678::/64"],
    ["::ffff:192.0.2.123", "unknown"],
    ["2001:::1234", "unknown"],
  ]) {
    const f = fixture();
    f.env.APPROVAL_METHODS = "host";
    const messages = hostNotifier(f);
    const held = f.gate.fetch(navigation(ip));
    await waitFor(() => messages.length === 1);
    assert.equal(messages[0].client, expected, ip);
    await f.gate.fetch(hostDecision(messages[0].id, false));
    assert.equal((await held).status, 404);
  }
});

test("explicit approval navigation sends a fresh request even with an approved gate cookie", async () => {
  const f = fixture();
  const approved = await approveNavigation(f);
  const cookie = approved.headers.get("Set-Cookie").split(";", 1)[0];
  const request = new Request("https://playfull-cat.example/?approval=request", {
    headers: { Accept: "text/html", Cookie: cookie, "CF-Connecting-IP": "203.0.113.7" },
  });
  const held = f.gate.fetch(request);
  await waitFor(() => f.notices.length === 2);
  await f.gate.fetch(decisionRequest(f.notices[1].body.actions[0]));
  const fresh = await held;
  assert.equal(fresh.status, 200);
  assert.notEqual(fresh.headers.get("Set-Cookie").split(";", 1)[0], cookie);
});

test("notifiers dispatch concurrently and host approval wins while ntfy publication is stalled", async () => {
  const f = fixture();
  f.env.APPROVAL_METHODS = "ntfy,host";
  let release;
  f.env.__gateFetch = async () => new Promise(resolve => { release = resolve; });
  const messages = hostNotifier(f);
  const held = f.gate.fetch(navigation());
  await waitFor(() => messages.length === 1 && release);
  await f.gate.fetch(hostDecision(messages[0].id, true));
  assert.equal((await held).status, 200);
  release(new Response(null, { status: 500 }));
  assert.equal(f.gate.pendingById.size, 0);
});

test("ntfy denial wins over host approval and an offline host leaves configured ntfy usable", async () => {
  const f = fixture();
  f.env.APPROVAL_METHODS = "ntfy,host";
  const messages = hostNotifier(f);
  const held = f.gate.fetch(navigation());
  await waitFor(() => messages.length === 1 && f.notices.length === 1);
  await f.gate.fetch(decisionRequest(f.notices[0].body.actions[1]));
  await f.gate.fetch(hostDecision(messages[0].id, true));
  assert.equal((await held).status, 404);
  assert.equal(f.assets.length, 0);
  const offline = fixture();
  offline.env.APPROVAL_METHODS = "host,ntfy";
  hostNotifier(offline, false);
  assert.equal((await approveNavigation(offline)).status, 200);
});

test("expired host and ntfy decisions cannot beat a delayed expiry timer", async () => {
  for (const method of ["host", "ntfy"]) {
    const f = fixture();
    f.env.APPROVAL_METHODS = method;
    const messages = hostNotifier(f);
    const held = f.gate.fetch(navigation());
    await waitFor(() => method === "host" ? messages.length === 1 : f.notices.length === 1);
    const pending = [...f.gate.pendingById.values()][0];
    pending.expiresAt = Date.now() - 1;
    if (method === "host") await f.gate.fetch(hostDecision(pending.id, true));
    else await f.gate.fetch(decisionRequest(f.notices[0].body.actions[0]));
    assert.equal((await held).status, 404);
    assert.equal(f.assets.length, 0);
  }
});

test("twenty simultaneous wrong guesses reserve the five-attempt budget before password work", async () => {
  const f = fixture();
  const approved = await approveNavigation(f);
  const cookie = approved.headers.get("Set-Cookie").split(";", 1)[0];
  const responses = await Promise.all(Array.from({ length: 20 }, () => f.gate.fetch(loginRequest(cookie, "wrong"))));
  assert.equal(responses.filter(response => response.status === 401).length, 4);
  assert.equal(responses.filter(response => response.status === 429).length, 16);
  const rates = await f.storage.list({ prefix: "auth-rate:" });
  assert.equal([...rates.values()][0].failures, 5);
  assert.equal([...rates.values()][0].inFlight, 0);
  assert.equal((await f.gate.fetch(loginRequest(cookie, f.env.APP_PASSWORD))).status, 429);
});

test("five concurrent correct passwords release the provisional lock even when the oldest reservation finishes last", async () => {
  const f = fixture();
  const approved = await approveNavigation(f);
  const cookie = approved.headers.get("Set-Cookie").split(";", 1)[0];
  const waitingBodies = [];
  const attempts = Array.from({ length: 5 }, () => {
    const request = loginRequest(cookie, f.env.APP_PASSWORD);
    const readBody = request.text.bind(request);
    request.text = async () => {
      await new Promise(resolve => waitingBodies.push(resolve));
      return readBody();
    };
    return f.gate.fetch(request);
  });
  await waitFor(() => waitingBodies.length === 5);
  const reserved = [...(await f.storage.list({ prefix: "auth-rate:" })).values()][0];
  assert.equal(reserved.failures, 5);
  assert.ok(reserved.lockedUntil > Date.now());
  waitingBodies.slice(1).forEach(resolve => resolve());
  await waitFor(() => [...f.storage.values.values()].filter(value => value.passwordTag).length === 4);
  waitingBodies[0]();
  assert.ok((await Promise.all(attempts)).every(response => response.status === 204));
  const settled = [...(await f.storage.list({ prefix: "auth-rate:" })).values()][0];
  assert.equal(settled.failures, 0);
  assert.equal(settled.inFlight, 0);
  assert.equal(settled.lockedUntil, 0);
  assert.equal((await f.gate.fetch(loginRequest(cookie, "wrong"))).status, 401);
});

test("socket identities require the original bound gate and auth sessions, including legacy binding", async () => {
  const f = fixture();
  const browser = await authenticatedBrowser(f);
  const identity = await (await f.gate.fetch(socketRequest(browser.cookie))).json();
  assert.match(identity.gateHash, /^[a-f0-9]{64}$/);
  assert.match(identity.authHash, /^[a-f0-9]{64}$/);
  assert.ok(identity.expiresAt > Date.now());
  assert.equal((await f.gate.fetch(internalPost("/__auth/socket", identity))).status, 204);
  const other = await approveNavigation(f, "198.51.100.9");
  const otherCookie = other.headers.get("Set-Cookie").split(";", 1)[0];
  assert.equal((await f.gate.fetch(socketRequest(`${otherCookie}; ${browser.appCookie}`))).status, 404);
  const authKey = `auth-session:${identity.authHash}`;
  const legacy = await f.storage.get(authKey);
  delete legacy.gateHash;
  await f.storage.put(authKey, legacy);
  assert.equal((await f.gate.fetch(socketRequest(browser.cookie))).status, 200);
  assert.equal((await f.gate.fetch(socketRequest(`${otherCookie}; ${browser.appCookie}`))).status, 404);
  await f.storage.delete(`session:${identity.gateHash}`);
  assert.equal((await f.gate.fetch(internalPost("/__auth/socket", identity))).status, 404);
});

test("logout and admin revocation notify durable signaling, media and file Room subscriptions", async () => {
  for (const action of ["logout", "forget-browser", "auth-sessions", "sessions", "one-session", "password"]) {
    const f = fixture();
    const browser = await authenticatedBrowser(f);
    const identity = await (await f.gate.fetch(socketRequest(browser.cookie))).json();
    const notified = [];
    for (const namespace of ["ROOM", "MEDIA_ROOM"]) f.env[namespace] = {
      idFromName: name => name, get: name => ({ async fetch(request) { notified.push([namespace, name, new URL(request.url).pathname]); return new Response(null, { status: 204 }); } }),
    };
    const rooms = [["ROOM", "room-v3"], ["MEDIA_ROOM", "media-v4:abcdef"], ["MEDIA_ROOM", "file-v1:abcdef"]];
    for (const [namespace, roomName] of rooms)
      assert.equal((await f.gate.fetch(internalPost("/__auth/socket-register", { namespace, roomName, identity }))).status, 204);
    // New actor demonstrates registration survives hibernation/restart.
    f.gate = new ApprovalGate({ storage: f.storage }, f.env);
    if (["logout", "forget-browser"].includes(action)) {
      await f.gate.fetch(new Request(`https://playfull-cat.example/__auth/${action}`, { method: "POST", headers: { Cookie: browser.cookie, Origin: "https://playfull-cat.example" } }));
    } else if (action === "password") {
      f.env.APP_PASSWORD = "rotated password";
      assert.equal((await f.gate.fetch(internalPost("/__auth/socket", identity))).status, 404);
      await waitFor(() => notified.length === 3);
    } else {
      let path = action;
      if (action === "one-session") {
        const sessions = await f.storage.list({ prefix: "session:" });
        path = `sessions/${[...sessions.values()][0].id}`;
      }
      await f.gate.fetch(new Request(`https://playfull-cat.example/__gate/manage/${path}`, { method: "DELETE", headers: { Authorization: "Bearer admin-token" } }));
    }
    assert.deepEqual(notified.map(([namespace, name]) => [namespace, name]).sort(), rooms.sort(), action);
    assert.ok(notified.every(([, , path]) => path === "/__auth/recheck"));
    assert.equal((await f.gate.fetch(internalPost("/__auth/socket", identity))).status, 404);
  }
});

test("Room subscriptions prune expired entries and unregister durable identities", async () => {
  const f = fixture();
  const browser = await authenticatedBrowser(f);
  const identity = await (await f.gate.fetch(socketRequest(browser.cookie))).json();
  await f.storage.put("socket-room:old", { expiresAt: Date.now() - 1 });
  const body = { namespace: "MEDIA_ROOM", roomName: "file-v1:abcdef", identity };
  assert.equal((await f.gate.fetch(internalPost("/__auth/socket-register", body))).status, 204);
  assert.equal((await f.storage.list({ prefix: "socket-room:" })).size, 1);
  assert.equal((await f.gate.fetch(internalPost("/__auth/socket-unregister", body))).status, 204);
  assert.equal((await f.storage.list({ prefix: "socket-room:" })).size, 0);
});

test("a Room registered after a concurrent logout broadcast receives its own recheck", async () => {
  const f = fixture();
  const browser = await authenticatedBrowser(f);
  const identity = await (await f.gate.fetch(socketRequest(browser.cookie))).json();
  const notified = [];
  f.env.MEDIA_ROOM = { idFromName: name => name, get: name => ({ fetch: async () => {
    notified.push(name); return new Response(null, { status: 204 });
  } }) };
  const originalPut = f.storage.put.bind(f.storage);
  let release;
  f.storage.put = async (key, value) => {
    if (key.startsWith("socket-room:")) await new Promise(resolve => { release = resolve; });
    return originalPut(key, value);
  };
  const registering = f.gate.fetch(internalPost("/__auth/socket-register", {
    namespace: "MEDIA_ROOM", roomName: "media-v4:abcdef", identity,
  }));
  await waitFor(() => release);
  assert.equal((await f.gate.fetch(new Request("https://playfull-cat.example/__auth/logout", {
    method: "POST", headers: { Cookie: browser.cookie, Origin: "https://playfull-cat.example" },
  }))).status, 204);
  assert.equal(notified.length, 0);
  release();
  assert.equal((await registering).status, 204);
  assert.deepEqual(notified, ["media-v4:abcdef"]);
  assert.equal((await f.gate.fetch(internalPost("/__auth/socket", identity))).status, 404);
});

test("approval reaches password login and remembered login creates a separate app session", async () => {
  const f = fixture();
  const approved = await approveNavigation(f);
  assert.equal(await approved.text(), "Sign in");
  const gateCookie = approved.headers.get("Set-Cookie").split(";", 1)[0];

  const login = await f.gate.fetch(loginRequest(gateCookie, "correct horse battery staple"));
  assert.equal(login.status, 204);
  assert.match(login.headers.get("Set-Cookie"), /__Host-bp-session=.+; Path=\/; Secure; HttpOnly; SameSite=Lax; Max-Age=2592000/);
  assert.match(login.headers.get("Set-Cookie"), /__Host-gate=.+; Path=\/; Secure; HttpOnly; SameSite=Lax; Max-Age=2592000/);
  const appCookie = login.headers.get("Set-Cookie").split(";", 1)[0];

  const check = await f.gate.fetch(new Request("https://gate.internal/__auth/check", { headers: { Cookie: `${gateCookie}; ${appCookie}` } }));
  assert.equal(check.status, 204);
  const app = await f.gate.fetch(new Request("https://playfull-cat.example/", { headers: { Cookie: `${gateCookie}; ${appCookie}` } }));
  assert.equal(await app.text(), "BubblePlay");
});

test("password failures are rate limited while a different network can still authenticate", async () => {
  const f = fixture();
  const approved = await approveNavigation(f);
  const gateCookie = approved.headers.get("Set-Cookie").split(";", 1)[0];
  for (let attempt = 0; attempt < 4; attempt += 1) {
    assert.equal((await f.gate.fetch(loginRequest(gateCookie, "wrong", true, "203.0.113.7"))).status, 401);
  }
  const locked = await f.gate.fetch(loginRequest(gateCookie, "wrong", true, "203.0.113.7"));
  assert.equal(locked.status, 429);
  assert.ok(Number(locked.headers.get("Retry-After")) > 0);
  assert.equal((await f.gate.fetch(loginRequest(gateCookie, "correct horse battery staple", true, "203.0.113.7"))).status, 429);

  const otherNetwork = await f.gate.fetch(loginRequest(gateCookie, "correct horse battery staple", false, "198.51.100.9"));
  assert.equal(otherNetwork.status, 204);
  assert.doesNotMatch(otherNetwork.headers.get("Set-Cookie"), /Max-Age=/);
});

test("holds one exact navigation until its approve capability is posted", async () => {
  const { gate, notices, assets } = fixture();
  let settled = false;
  const held = gate.fetch(navigation()).then((response) => { settled = true; return response; });
  await waitFor(() => notices.length === 1);
  assert.equal(settled, false);
  assert.equal(notices.length, 1);
  assert.equal(assets.length, 0);

  const approve = notices[0].body.actions.find((action) => action.label === "Approve");
  const callback = await gate.fetch(decisionRequest(approve));
  assert.equal(callback.status, 204);
  const response = await held;
  assert.equal(await response.text(), "Sign in");
  assert.match(response.headers.get("Set-Cookie"), /^__Host-gate=.+; Path=\/; Secure; HttpOnly; SameSite=Lax$/);
  assert.deepEqual(assets, ["/login.html"]);

  const replay = await gate.fetch(decisionRequest(approve));
  assert.equal(replay.status, 204);
  assert.deepEqual(assets, ["/login.html"]);
});

test("approval releases only the request carrying that notification's capability", async () => {
  const { gate, notices } = fixture();
  const first = gate.fetch(navigation("203.0.113.1"));
  // Notification order follows asynchronous crypto, not invocation order.
  await waitFor(() => notices.length === 1);
  const second = gate.fetch(navigation("198.51.100.2"));
  await waitFor(() => notices.length === 2);
  const firstApprove = notices[0].body.actions.find((action) => action.label === "Approve");
  await gate.fetch(decisionRequest(firstApprove));
  assert.equal((await first).status, 200);
  assert.equal(await Promise.race([second.then(() => "settled"), new Promise((resolve) => setTimeout(() => resolve("pending"), 10))]), "pending");
  const secondDeny = notices[1].body.actions.find((action) => action.label === "Deny");
  await gate.fetch(decisionRequest(secondDeny));
  assert.equal((await second).status, 404);
});

test("valid gate and app cookies form a notification-free fast path and gate revocation wins", async () => {
  const { gate, notices } = fixture();
  const held = gate.fetch(navigation());
  await waitFor(() => notices.length === 1);
  const approve = notices[0].body.actions.find((action) => action.label === "Approve");
  await gate.fetch(decisionRequest(approve));
  const approved = await held;
  const gateCookie = approved.headers.get("Set-Cookie").split(";", 1)[0];
  const login = await gate.fetch(loginRequest(gateCookie, "correct horse battery staple"));
  const appCookie = login.headers.get("Set-Cookie").split(";", 1)[0];
  const cookie = `${gateCookie}; ${appCookie}`;

  const fast = await gate.fetch(new Request("https://playfull-cat.example/input.js", { headers: { Cookie: cookie } }));
  assert.equal(fast.status, 200);
  assert.equal(notices.length, 1);
  const checked = await gate.fetch(new Request("https://gate.internal/__gate/check", { headers: { Cookie: cookie } }));
  assert.equal(checked.status, 204);
  const appChecked = await gate.fetch(new Request("https://gate.internal/__auth/check", { headers: { Cookie: cookie } }));
  assert.equal(appChecked.status, 204);

  const authListed = await gate.fetch(new Request("https://playfull-cat.example/__gate/manage/auth-sessions", { headers: { Authorization: "Bearer admin-token" } }));
  assert.equal((await authListed.json()).length, 1);
  assert.equal((await gate.fetch(new Request("https://playfull-cat.example/__gate/manage/auth-sessions", { method: "DELETE", headers: { Authorization: "Bearer admin-token" } }))).status, 204);
  assert.equal((await gate.fetch(new Request("https://gate.internal/__auth/check", { headers: { Cookie: cookie } }))).status, 404);
  assert.equal((await gate.fetch(new Request("https://gate.internal/__gate/check", { headers: { Cookie: cookie } }))).status, 204);

  const listed = await gate.fetch(new Request("https://playfull-cat.example/__gate/manage/sessions", { headers: { Authorization: "Bearer admin-token" } }));
  const sessions = await listed.json();
  assert.equal(sessions.length, 1);
  const revoked = await gate.fetch(new Request(`https://playfull-cat.example/__gate/manage/sessions/${sessions[0].id}`, { method: "DELETE", headers: { Authorization: "Bearer admin-token" } }));
  assert.equal(revoked.status, 204);
  const after = await gate.fetch(new Request("https://gate.internal/__gate/check", { headers: { Cookie: cookie } }));
  assert.equal(after.status, 404);
});

test("remembered sessions survive external top-level navigation and password rotation invalidates only app auth", async () => {
  const f = fixture();
  const approved = await approveNavigation(f);
  const gateCookie = approved.headers.get("Set-Cookie").split(";", 1)[0];
  const login = await f.gate.fetch(loginRequest(gateCookie, "correct horse battery staple", true));
  const appCookie = login.headers.get("Set-Cookie").match(/__Host-bp-session=[^;,]+/)?.[0];
  assert.ok(appCookie);
  const cookie = `${gateCookie}; ${appCookie}`;
  assert.equal((await f.gate.fetch(new Request("https://playfull-cat.example/", { headers: { Cookie: cookie } }))).status, 200);
  f.env.APP_PASSWORD = "rotated";
  assert.equal((await f.gate.fetch(new Request("https://gate.internal/__auth/check", { headers: { Cookie: cookie } }))).status, 404);
  assert.equal((await f.gate.fetch(new Request("https://gate.internal/__gate/check", { headers: { Cookie: cookie } }))).status, 204);
});

test("approved browsers can load only the login design assets before password authentication", async () => {
  const f = fixture();
  const approved = await approveNavigation(f);
  const gateCookie = approved.headers.get("Set-Cookie").split(";", 1)[0];

  const anonymousStyle = await f.gate.fetch(new Request("https://playfull-cat.example/bubbleplay.css"));
  assert.equal(anonymousStyle.status, 404);

  const style = await f.gate.fetch(new Request("https://playfull-cat.example/bubbleplay.css", { headers: { Cookie: gateCookie } }));
  assert.equal(style.status, 200);
  assert.equal(style.headers.get("Cache-Control"), "no-store");

  const font = await f.gate.fetch(new Request("https://playfull-cat.example/fonts/atkinson-hyperlegible-next.woff2", { headers: { Cookie: gateCookie } }));
  assert.equal(font.status, 200);
  assert.equal(font.headers.get("Cache-Control"), "public, max-age=31536000, immutable");

  const script = await f.gate.fetch(new Request("https://playfull-cat.example/input.js", { headers: { Cookie: gateCookie } }));
  assert.equal(script.status, 404);
});

test("an authenticated browser can load the receiver module graph from static assets", async () => {
  const f = fixture();
  const browser = await authenticatedBrowser(f);
  // The receiver imports /monitor-selection.js by absolute path, so the
  // production asset passthrough must serve it exactly like /receiver.js.
  for (const path of ["/receiver.js", "/monitor-selection.js", "/transport.js"]) {
    const response = await f.gate.fetch(new Request(`https://playfull-cat.example${path}`, {
      headers: { Cookie: browser.cookie },
    }));
    assert.equal(response.status, 200, `${path} must be served`);
  }
  assert.deepEqual(f.assets.slice(-3), ["/receiver.js", "/monitor-selection.js", "/transport.js"]);
});

test("pre-signaling client diagnostics require app authentication and retain a readable timeline", async () => {
  const f = fixture();
  assert.equal((await f.gate.fetch(diagnosticRequest("", diagnosticEvent(1)))).status, 404);

  const browser = await authenticatedBrowser(f);
  assert.equal((await f.gate.fetch(diagnosticRequest(browser.cookie, diagnosticEvent(1)))).status, 204);
  assert.equal((await f.gate.fetch(diagnosticRequest(browser.cookie, diagnosticEvent(2, "module-ready")))).status, 204);

  const timeline = await f.gate.fetch(new Request(
    `https://playfull-cat.example/__diag/client/${DEBUG_ATTEMPT}`,
    { headers: { Cookie: browser.cookie } },
  ));
  assert.equal(timeline.status, 200);
  const body = await timeline.json();
  assert.equal(body.attemptId, DEBUG_ATTEMPT);
  assert.deepEqual(body.events.map(event => [event.seq, event.stage]), [
    [1, "bootstrap-inline"],
    [2, "module-ready"],
  ]);
  assert.ok(body.events.every(event => Number.isInteger(event.serverAtMs)));
});

test("client diagnostics retain compatibility skew instead of rejecting the evidence", async () => {
  const f = fixture();
  const browser = await authenticatedBrowser(f);
  const skewed = {
    ...diagnosticEvent(1),
    compatibilityId: "stale-receiver-contract",
    receiverBuild: "stale-receiver-build",
  };
  assert.equal((await f.gate.fetch(diagnosticRequest(browser.cookie, skewed))).status, 204);
  const timeline = await f.gate.fetch(new Request(
    `https://playfull-cat.example/__diag/client/${DEBUG_ATTEMPT}`,
    { headers: { Cookie: browser.cookie } },
  ));
  const body = await timeline.json();
  assert.equal(body.events[0].compatibilityId, "stale-receiver-contract");
  assert.equal(body.events[0].compatibilityMatch, false);
  assert.equal(body.events[0].receiverBuild, "stale-receiver-build");
  assert.equal(body.events[0].receiverBuildMatch, false);
});

test("client diagnostic ingestion rejects untrusted shapes and remains bounded", async () => {
  const f = fixture();
  const browser = await authenticatedBrowser(f);
  const wrongOrigin = await f.gate.fetch(diagnosticRequest(browser.cookie, diagnosticEvent(1), "https://attacker.example"));
  assert.equal(wrongOrigin.status, 404);
  const unknownField = await f.gate.fetch(diagnosticRequest(browser.cookie, { ...diagnosticEvent(1), secret: "no" }));
  assert.equal(unknownField.status, 400);
  const nestedDetail = await f.gate.fetch(diagnosticRequest(browser.cookie, {
    ...diagnosticEvent(1), detail: { nested: { token: "must-not-be-accepted" } },
  }));
  assert.equal(nestedDetail.status, 400);

  for (let seq = 1; seq <= 132; seq++) {
    assert.equal((await f.gate.fetch(diagnosticRequest(browser.cookie, diagnosticEvent(seq)))).status, 204);
  }
  const timeline = await f.gate.fetch(new Request(
    `https://playfull-cat.example/__diag/client/${DEBUG_ATTEMPT}`,
    { headers: { Cookie: browser.cookie } },
  ));
  const body = await timeline.json();
  assert.equal(body.events.length, 128);
  assert.equal(body.events[0].seq, 5);
  assert.equal(body.events.at(-1).seq, 132);
});

test("logout requires same-origin POST and revokes only the current app session", async () => {
  const f = fixture();
  const browser = await authenticatedBrowser(f);

  const missingOrigin = await f.gate.fetch(new Request("https://playfull-cat.example/__auth/logout", {
    method: "POST",
    headers: { Cookie: browser.cookie },
  }));
  assert.equal(missingOrigin.status, 404);

  const rejected = await f.gate.fetch(new Request("https://playfull-cat.example/__auth/logout", {
    method: "POST",
    headers: { Cookie: browser.cookie, Origin: "https://attacker.example" },
  }));
  assert.equal(rejected.status, 404);
  assert.equal((await f.gate.fetch(new Request("https://gate.internal/__auth/check", { headers: { Cookie: browser.cookie } }))).status, 204);

  const logout = await f.gate.fetch(new Request("https://playfull-cat.example/__auth/logout", {
    method: "POST",
    headers: { Cookie: browser.cookie, Origin: "https://playfull-cat.example" },
  }));
  assert.equal(logout.status, 204);
  assert.match(logout.headers.get("Set-Cookie"), /__Host-bp-session=; Path=\/; Secure; HttpOnly; SameSite=Lax; Max-Age=0/);
  assert.equal((await f.gate.fetch(new Request("https://gate.internal/__auth/check", { headers: { Cookie: browser.cookie } }))).status, 404);
  assert.equal((await f.gate.fetch(new Request("https://gate.internal/__gate/check", { headers: { Cookie: browser.cookie } }))).status, 204);
});

test("forget browser revokes both current sessions and clears both cookies", async () => {
  const f = fixture();
  const browser = await authenticatedBrowser(f);

  const forgotten = await f.gate.fetch(new Request("https://playfull-cat.example/__auth/forget-browser", {
    method: "POST",
    headers: { Cookie: browser.cookie, Origin: "https://playfull-cat.example" },
  }));
  assert.equal(forgotten.status, 204);
  const cookies = forgotten.headers.get("Set-Cookie");
  assert.match(cookies, /__Host-bp-session=; Path=\/; Secure; HttpOnly; SameSite=Lax; Max-Age=0/);
  assert.match(cookies, /__Host-gate=; Path=\/; Secure; HttpOnly; SameSite=Lax; Max-Age=0/);
  assert.equal((await f.gate.fetch(new Request("https://gate.internal/__auth/check", { headers: { Cookie: browser.cookie } }))).status, 404);
  assert.equal((await f.gate.fetch(new Request("https://gate.internal/__gate/check", { headers: { Cookie: browser.cookie } }))).status, 404);
});

test("probes never notify and a same-prefix parallel navigation is bounded", async () => {
  const { gate, notices } = fixture();
  assert.equal((await gate.fetch(new Request("https://playfull-cat.example/admin"))).status, 404);
  assert.equal((await gate.fetch(new Request("https://playfull-cat.example/ws", { headers: { Upgrade: "websocket" } }))).status, 404);
  assert.equal(notices.length, 0);

  const first = gate.fetch(navigation("203.0.113.7"));
  await waitFor(() => notices.length === 1);
  const duplicate = await gate.fetch(navigation("203.0.113.99"));
  assert.equal(duplicate.status, 404);
  assert.equal(notices.length, 1);
  const deny = notices[0].body.actions.find((action) => action.label === "Deny");
  await gate.fetch(decisionRequest(deny));
  await first;
});

test("disconnect and expiry invalidate capabilities without reaching assets", async () => {
  const disconnected = fixture();
  const controller = new AbortController();
  const held = disconnected.gate.fetch(new Request(navigation(), { signal: controller.signal }));
  await waitFor(() => disconnected.notices.length === 1);
  const lateApprove = disconnected.notices[0].body.actions.find((action) => action.label === "Approve");
  controller.abort();
  assert.equal((await held).status, 404);
  assert.equal((await disconnected.gate.fetch(decisionRequest(lateApprove))).status, 204);
  assert.deepEqual(disconnected.assets, []);

  const expired = fixture();
  expired.gate.env.__gateApprovalTtlMs = 10;
  const expiredResponse = await expired.gate.fetch(navigation("198.51.100.8"));
  assert.equal(expiredResponse.status, 404);
  assert.deepEqual(expired.assets, []);
  assert.equal(expired.storage.values.size, 0);
});

test("ntfy failure is fail-closed and the global pending limit is bounded", async () => {
  const failed = fixture();
  failed.gate.env.__gateFetch = async () => new Response(null, { status: 503 });
  assert.equal((await failed.gate.fetch(navigation())).status, 404);
  assert.deepEqual(failed.assets, []);
  assert.equal(failed.storage.values.size, 0);

  const bounded = fixture();
  const held = [];
  for (let i = 0; i < 10; i += 1) held.push(bounded.gate.fetch(navigation(`10.${i}.0.1`)));
  await waitFor(() => bounded.notices.length === 10);
  assert.equal((await bounded.gate.fetch(navigation("192.0.2.1"))).status, 404);
  assert.equal(bounded.notices.length, 10);
  for (const notice of bounded.notices) {
    const deny = notice.body.actions.find((action) => action.label === "Deny");
    await bounded.gate.fetch(decisionRequest(deny));
  }
  await Promise.all(held);
});

test("a simultaneous burst cannot take more than one slot per prefix or exceed the global cap", async (t) => {
  const samePrefix = fixture();
  const burst = [];
  for (let i = 1; i <= 5; i += 1) burst.push(samePrefix.gate.fetch(navigation(`203.0.113.${i}`)));
  await waitFor(() => samePrefix.notices.length >= 1, 10_000);
  const refused = await Promise.all(burst.slice(1));
  assert.deepEqual(refused.map((response) => response.status), [404, 404, 404, 404]);
  assert.equal(samePrefix.notices.length, 1);
  assert.equal(samePrefix.gate.pendingById.size, 1);
  await samePrefix.gate.fetch(decisionRequest(samePrefix.notices[0].body.actions.find((action) => action.label === "Deny")));
  assert.equal((await burst[0]).status, 404);

  const capped = fixture();
  // Hashing may outlast the short fixture lease on a busy machine. Control
  // expiry explicitly while polling real asynchronous approval publication.
  const pollTimer = setTimeout;
  t.mock.timers.enable({ apis: ["setTimeout"] });
  capped.gate.env.__gateApprovalTtlMs = 150;
  const held = [];
  for (let i = 0; i < 14; i += 1) held.push(capped.gate.fetch(navigation(`10.${i}.0.1`)));
  await waitFor(() => capped.notices.length >= 10, 10_000, pollTimer);
  assert.deepEqual((await Promise.all(held.slice(10))).map((response) => response.status), [404, 404, 404, 404]);
  assert.equal(capped.notices.length, 10);
  assert.equal(capped.gate.pendingById.size, 10);

  // Expiry frees every slot, capability and prefix at the deadline, so a new
  // network is held again straight away.
  t.mock.timers.tick(150);
  assert.deepEqual((await Promise.all(held)).map((response) => response.status), Array(14).fill(404));
  assert.equal(capped.gate.pendingById.size, 0);
  assert.equal(capped.gate.pendingByCapability.size, 0);
  assert.equal(capped.gate.pendingPrefixes.size, 0);
  const next = capped.gate.fetch(navigation("192.0.2.1"));
  await waitFor(() => capped.notices.length === 11, 10_000, pollTimer);
  assert.equal(capped.gate.pendingById.size, 1);
  t.mock.timers.tick(150);
  assert.equal((await next).status, 404);
  assert.equal(capped.gate.pendingById.size, 0);
});

test("edge routing keeps host authorization separate and sends all browser HTTP through the gate", async () => {
  const forwarded = [];
  const gate = {
    async fetch(request) {
      forwarded.push(new URL(request.url).pathname);
      return new Response("gated", { status: 418 });
    },
  };
  const env = {
    ...WORKER_SECRETS,
    GATE: { idFromName: (name) => name, get: () => gate },
  };

  const asset = await worker.fetch(new Request("https://playfull-cat.example/input.js"), env);
  assert.equal(asset.status, 418);
  assert.deepEqual(forwarded, ["/input.js"]);

  const hostWithoutBearer = await worker.fetch(new Request("https://playfull-cat.example/host-ws"), env);
  assert.equal(hostWithoutBearer.status, 404);
  assert.deepEqual(forwarded, ["/input.js"]);

  const browserWsWithoutCookie = await worker.fetch(new Request("https://playfull-cat.example/ws"), env);
  assert.equal(browserWsWithoutCookie.status, 404);
  assert.deepEqual(forwarded, ["/input.js", "/__auth/check"]);
});

test("pinned browser sockets preserve the Worker version override across the internal gate check", async () => {
  const versionOverride = 'playfull-cat="a34966a1-d238-4dec-8dbe-5405f6a9fdce"';
  const gateRequests = [];
  const signalResponse = new Response("signal", { status: 200 });
  const env = {
    ...WORKER_SECRETS,
    GATE: {
      idFromName: () => "gate",
      get: () => ({
        async fetch(request) {
          gateRequests.push(request);
          return new Response(null, { status: 204 });
        },
      }),
    },
    ROOM: { idFromName: name => name, get: () => ({ fetch: async () => signalResponse }) },
  };
  const run = "123e4567-e89b-42d3-a456-426614174000";
  const compat = "wss-v4-bench2-hevc-tiles-bpip4-v16";
  const response = await worker.fetch(new Request(
    `https://playfull-cat.example/lab-ws?run=${run}&compat=${compat}`,
    {
      headers: {
        Upgrade: "websocket",
        Cookie: "remembered=1",
        "Cloudflare-Workers-Version-Overrides": versionOverride,
      },
    },
  ), env);

  assert.equal(response, signalResponse);
  assert.equal(gateRequests.length, 1);
  assert.equal(gateRequests[0].headers.get("Cookie"), "remembered=1");
  assert.equal(gateRequests[0].headers.get("Cloudflare-Workers-Version-Overrides"), versionOverride);
});

test("explicit local Stream Lab mode bypasses auth only for valid loopback lab sockets", async () => {
  const selected = [];
  const signalResponse = new Response("signal", { status: 200 });
  const assetResponse = new Response("asset", { status: 200 });
  const env = {
    ...WORKER_SECRETS,
    STREAM_LAB_LOCAL_BYPASS: "1",
    ASSETS: { fetch: async () => assetResponse },
    GATE: {
      idFromName: () => "gate",
      get: () => ({ fetch: async () => new Response(null, { status: 404 }) }),
    },
    ROOM: {
      idFromName: name => {
        selected.push(name);
        return name;
      },
      get: () => ({ fetch: async () => signalResponse }),
    },
    MEDIA_ROOM: {
      idFromName: name => name,
      get: () => ({ fetch: async () => new Response("media", { status: 200 }) }),
    },
  };
  const run = "123e4567-e89b-42d3-a456-426614174000";
  const compat = "wss-v4-bench2-hevc-tiles-bpip4-v16";

  assert.equal(await worker.fetch(new Request("http://127.0.0.1:8787/"), env), assetResponse);
  assert.equal(await worker.fetch(
    new Request(`http://127.0.0.1:8787/lab-ws?run=${run}&compat=${compat}`, { headers: { Upgrade: "websocket" } }),
    env,
  ), signalResponse);
  assert.equal(await worker.fetch(
    new Request(`http://localhost:8787/lab-host-ws?run=${run}&compat=${compat}`, { headers: { Upgrade: "websocket" } }),
    env,
  ), signalResponse);
  assert.deepEqual(selected, [`lab-signal-v1:${run}`, `lab-signal-v1:${run}`]);

  assert.equal((await worker.fetch(
    new Request(`https://playfull-cat.example/lab-ws?run=${run}&compat=${compat}`, { headers: { Upgrade: "websocket" } }),
    env,
  )).status, 404);
  assert.equal((await worker.fetch(new Request("https://playfull-cat.example/"), env)).status, 404);
  assert.equal((await worker.fetch(
    new Request("http://127.0.0.1:8787/ws", { headers: { Upgrade: "websocket" } }),
    env,
  )).status, 404);
  assert.equal((await worker.fetch(
    new Request("http://127.0.0.1:8787/lab-ws?run=not-a-uuid", { headers: { Upgrade: "websocket" } }),
    env,
  )).status, 404);
  assert.equal(selected.length, 2);

  // The bypass itself needs no relay secrets, and stays off unless the
  // variable is exactly "1".
  const { ASSETS, GATE, ROOM, MEDIA_ROOM } = env;
  const bare = { STREAM_LAB_LOCAL_BYPASS: "1", ASSETS, GATE, ROOM, MEDIA_ROOM };
  const labSocket = `http://127.0.0.1:8787/lab-ws?run=${run}&compat=${compat}`;
  assert.equal(await worker.fetch(new Request(labSocket, { headers: { Upgrade: "websocket" } }), bare), signalResponse);
  for (const value of [undefined, "", "0", "true", 1]) {
    const off = { ...env, STREAM_LAB_LOCAL_BYPASS: value };
    assert.equal((await worker.fetch(new Request(labSocket, { headers: { Upgrade: "websocket" } }), off)).status, 404, String(value));
    assert.equal((await worker.fetch(new Request("http://127.0.0.1:8787/"), off)).status, 404, String(value));
  }
});

test("a loopback hostname never enables the lab bypass for a client a proxy reports as remote", async () => {
  let rooms = 0;
  const assetResponse = new Response("asset", { status: 200 });
  const signalResponse = new Response("signal", { status: 200 });
  const env = {
    ...WORKER_SECRETS,
    STREAM_LAB_LOCAL_BYPASS: "1",
    ASSETS: { fetch: async () => assetResponse },
    GATE: { idFromName: () => "gate", get: () => ({ fetch: async () => new Response(null, { status: 404 }) }) },
    ROOM: { idFromName: name => name, get: () => ({ fetch: async () => { rooms += 1; return signalResponse; } }) },
  };
  const run = "123e4567-e89b-42d3-a456-426614174000";
  const compat = "wss-v4-bench2-hevc-tiles-bpip4-v16";
  const socket = (path, headers) => new Request(`http://localhost:8787/${path}?run=${run}&compat=${compat}`,
    { headers: { Upgrade: "websocket", ...headers } });

  for (const headers of [
    { "CF-Connecting-IP": "203.0.113.7" },
    { "True-Client-IP": "203.0.113.7" },
    { "X-Real-IP": "2001:db8::1" },
    { "X-Forwarded-For": "203.0.113.7" },
    { "X-Forwarded-For": "127.0.0.1, 203.0.113.7" },
    { "X-Forwarded-For": "" },
    { Forwarded: "for=203.0.113.7;proto=http" },
    { Forwarded: 'for="[2001:db8::1]:4711", for=127.0.0.1' },
    { Forwarded: "proto=http" },
    { "CF-Connecting-IP": "127.0.0.1", "X-Forwarded-For": "203.0.113.7" },
  ]) {
    const label = JSON.stringify(headers);
    assert.equal((await worker.fetch(socket("lab-ws", headers), env)).status, 404, label);
    assert.equal((await worker.fetch(socket("lab-host-ws", headers), env)).status, 404, label);
    assert.equal((await worker.fetch(new Request("http://localhost:8787/", { headers }), env)).status, 404, label);
  }
  assert.equal(rooms, 0);

  for (const headers of [
    { "CF-Connecting-IP": "127.0.0.1" },
    { "CF-Connecting-IP": "::1" },
    { "X-Forwarded-For": "127.0.0.1, ::ffff:127.0.0.1" },
    { Forwarded: 'for="[::1]:4711";proto=http' },
  ]) {
    assert.equal(await worker.fetch(socket("lab-ws", headers), env), signalResponse, JSON.stringify(headers));
    assert.equal(await worker.fetch(new Request("http://localhost:8787/", { headers }), env), assetResponse);
  }
});

test("socket upgrades from another origin are refused before authentication", async () => {
  let gateChecks = 0;
  let rooms = 0;
  const roomResponse = new Response("room", { status: 200 });
  const env = {
    ...WORKER_SECRETS,
    STREAM_LAB_LOCAL_BYPASS: "1",
    // The default /ws asks the GATE which PC to use; this mock registers none.
    GATE: { idFromName: () => "gate", get: () => ({ fetch: async (request) => {
      if (new URL(request.url).pathname === "/__pc/resolve") return Response.json({ pc: null });
      gateChecks += 1; return new Response(null, { status: 204 });
    } }) },
    ROOM: { idFromName: name => name, get: () => ({ fetch: async () => { rooms += 1; return roomResponse; } }) },
    MEDIA_ROOM: { idFromName: name => name, get: () => ({ fetch: async () => { rooms += 1; return roomResponse; } }) },
  };
  const epoch = "0123456789abcdef0123456789abcdef";
  const run = "123e4567-e89b-42d3-a456-426614174000";
  const compat = "wss-v4-bench2-hevc-tiles-bpip4-v16";
  const browser = (url, origin) => new Request(url, {
    headers: { Upgrade: "websocket", Cookie: "remembered=1", ...(origin === undefined ? {} : { Origin: origin }) },
  });
  const paths = ["/ws", `/media-ws?epoch=${epoch}`, `/file-ws?epoch=${epoch}`,
    `/lab-ws?run=${run}&compat=${compat}`, `/lab-media-ws?run=${run}&compat=${compat}&epoch=${epoch}`,
    `/lab-file-ws?run=${run}&compat=${compat}&epoch=${epoch}`];

  for (const path of paths) {
    for (const origin of ["https://attacker.example", "http://relay.example", "https://relay.example:8443",
      "https://relay.example.attacker.example", "null", ""]) {
      const response = await worker.fetch(browser(`https://relay.example${path}`, origin), env);
      assert.equal(response.status, 404, `${path} from ${origin}`);
    }
  }
  assert.equal(gateChecks, 0);
  assert.equal(rooms, 0);

  // The page's own origin, and native clients that send no Origin (Android).
  for (const path of paths) {
    assert.equal(await worker.fetch(browser(`https://relay.example${path}`, "https://relay.example"), env), roomResponse, path);
    assert.equal(await worker.fetch(browser(`https://relay.example${path}`), env), roomResponse, path);
  }
  assert.equal(gateChecks, paths.length * 2);

  // Host sockets authenticate with the bearer credential and send no Origin;
  // a foreign Origin is refused there too.
  const hostHeaders = { Upgrade: "websocket", Authorization: `Bearer ${RELAY_SECRETS.HOST_GATE_TOKEN}` };
  assert.equal(await worker.fetch(new Request("https://relay.example/host-ws", { headers: hostHeaders }), env), roomResponse);
  assert.equal((await worker.fetch(new Request("https://relay.example/host-ws",
    { headers: { ...hostHeaders, Origin: "https://attacker.example" } }), env)).status, 404);

  // A page on another site cannot reach the credential-free local lab sockets.
  for (const path of ["lab-ws", "lab-host-ws"]) {
    const local = `http://127.0.0.1:8787/${path}?run=${run}&compat=${compat}`;
    assert.equal((await worker.fetch(browser(local, "https://attacker.example"), env)).status, 404);
    assert.equal(await worker.fetch(browser(local, "http://127.0.0.1:8787"), env), roomResponse);
  }
});

test("the relay refuses to operate with missing or short secrets and never echoes their values", async () => {
  assert.deepEqual(weakSecretNames(RELAY_SECRETS), []);
  assert.deepEqual(weakSecretNames({}), ["TOKEN", "HOST_GATE_TOKEN", "GATE_ADMIN_TOKEN", "APP_PASSWORD"]);
  assert.deepEqual(weakSecretNames({ ...WORKER_SECRETS, TOKEN: "x".repeat(32), APP_PASSWORD: "y".repeat(12) }), []);

  let reached = 0;
  const backend = { fetch: async () => { reached += 1; return new Response(null, { status: 204 }); } };
  const bindings = {
    GATE: { idFromName: () => "gate", get: () => backend },
    ROOM: { idFromName: name => name, get: () => backend },
    MEDIA_ROOM: { idFromName: name => name, get: () => backend },
  };
  const requests = () => [
    new Request("https://relay.example/", { headers: { Accept: "text/html" } }),
    new Request("https://relay.example/__auth/login", { method: "POST", body: "{}" }),
    new Request("https://relay.example/__gate/manage/sessions", { headers: { Authorization: "Bearer short" } }),
    new Request("https://relay.example/ws", { headers: { Upgrade: "websocket", Cookie: "remembered=1" } }),
    new Request("https://relay.example/host-ws", { headers: { Upgrade: "websocket", Authorization: "Bearer short" } }),
  ];
  for (const [name, value] of [
    ["TOKEN", "t".repeat(31)], ["TOKEN", undefined], ["TOKEN", 12345678901234567890123456789012n],
    ["HOST_GATE_TOKEN", "short-host-credential"], ["HOST_GATE_TOKEN", ""],
    ["GATE_ADMIN_TOKEN", "a".repeat(31)],
    ["APP_PASSWORD", "elevenchars"], ["APP_PASSWORD", undefined],
  ]) {
    const env = { ...WORKER_SECRETS, ...bindings, [name]: value };
    for (const request of requests()) {
      const response = await worker.fetch(request, env);
      const body = await response.text();
      assert.equal(response.status, 500, `${name} ${request.url}`);
      assert.equal(response.headers.get("Cache-Control"), "no-store");
      assert.match(body, /^Relay configuration error: /);
      assert.ok(body.includes(name));
      for (const secret of Object.values(env)) {
        if (typeof secret === "string" && secret) assert.equal(body.includes(secret), false, `${name} leaked a value`);
      }
    }
  }
  assert.equal(reached, 0);

  const everything = await (await worker.fetch(requests()[0], { ...bindings })).text();
  for (const name of ["TOKEN", "HOST_GATE_TOKEN", "GATE_ADMIN_TOKEN", "APP_PASSWORD"]) assert.ok(everything.includes(name));

  const healthy = { ...WORKER_SECRETS, ...bindings };
  for (const request of requests().slice(0, 3)) assert.equal((await worker.fetch(request, healthy)).status, 204);
  assert.equal(reached, 3);
});

test("media websocket routing uses an isolated Durable Object namespace", async () => {
  const selected = [];
  const mediaResponse = new Response("media", { status: 200 });
  const env = {
    ...WORKER_SECRETS,
    GATE: { idFromName: () => "gate", get: () => ({ fetch: async () => new Response(null, { status: 204 }) }) },
    ROOM: { idFromName: (name) => { selected.push(`signal:${name}`); return name; }, get: () => ({ fetch: async () => new Response("signal") }) },
    MEDIA_ROOM: { idFromName: (name) => { selected.push(`media:${name}`); return name; }, get: () => ({ fetch: async () => mediaResponse }) },
  };
  const epoch = "0123456789abcdef0123456789abcdef";
  const response = await worker.fetch(new Request(`https://playfull-cat.example/media-ws?epoch=${epoch}`, {
    headers: { Upgrade: "websocket", Cookie: "remembered=1" },
  }), env);
  assert.equal(response, mediaResponse);
  assert.deepEqual(selected, [`media:media-v4:${epoch}`]);
  // One media Durable Object per epoch: a media socket without an epoch is not routed.
  const missing = await worker.fetch(new Request("https://playfull-cat.example/media-ws", {
    headers: { Upgrade: "websocket", Cookie: "remembered=1" },
  }), env);
  assert.equal(missing.status, 404);
  assert.equal(selected.length, 1);
});

test("lab websocket routing is isolated by canonical run id without changing production rooms", async () => {
  const selected = [];
  const signalResponse = new Response("signal", { status: 200 });
  const mediaResponse = new Response("media", { status: 200 });
  const env = {
    ...WORKER_SECRETS,
    GATE: { idFromName: () => "gate", get: () => ({ fetch: async () => new Response(null, { status: 204 }) }) },
    ROOM: { idFromName: (name) => { selected.push(`signal:${name}`); return name; }, get: () => ({ fetch: async () => signalResponse }) },
    MEDIA_ROOM: { idFromName: (name) => { selected.push(`media:${name}`); return name; }, get: () => ({ fetch: async () => mediaResponse }) },
  };
  const runA = "123e4567-e89b-42d3-a456-426614174000";
  const runB = "123e4567-e89b-42d3-a456-426614174001";
  const epoch = "0123456789abcdef0123456789abcdef";
  const compat = "wss-v4-bench2-hevc-tiles-bpip4-v16";
  const browserHeaders = { Upgrade: "websocket", Cookie: "remembered=1" };
  const hostHeaders = { Upgrade: "websocket", Authorization: `Bearer ${RELAY_SECRETS.HOST_GATE_TOKEN}` };

  assert.equal(await worker.fetch(new Request(`https://playfull-cat.example/lab-ws?run=${runA}&compat=${compat}`, { headers: browserHeaders }), env), signalResponse);
  assert.equal(await worker.fetch(new Request(`https://playfull-cat.example/lab-host-ws?run=${runA}&compat=${compat}`, { headers: hostHeaders }), env), signalResponse);
  assert.equal(await worker.fetch(new Request(`https://playfull-cat.example/lab-media-ws?run=${runA}&compat=${compat}&epoch=${epoch}`, { headers: browserHeaders }), env), mediaResponse);
  assert.equal(await worker.fetch(new Request(`https://playfull-cat.example/lab-host-media-ws?run=${runA}&compat=${compat}&epoch=${epoch}`, { headers: hostHeaders }), env), mediaResponse);
  assert.equal(await worker.fetch(new Request(`https://playfull-cat.example/lab-ws?run=${runB}&compat=${compat}`, { headers: browserHeaders }), env), signalResponse);

  assert.deepEqual(selected, [
    `signal:lab-signal-v1:${runA}`,
    `signal:lab-signal-v1:${runA}`,
    `media:lab-media-v4:${runA}:${epoch}`,
    `media:lab-media-v4:${runA}:${epoch}`,
    `signal:lab-signal-v1:${runB}`,
  ]);

  const beforeInvalid = selected.length;
  assert.equal((await worker.fetch(new Request("https://playfull-cat.example/lab-ws?run=not-a-uuid", { headers: browserHeaders }), env)).status, 404);
  assert.equal((await worker.fetch(new Request(`https://playfull-cat.example/lab-media-ws?run=${runA}&compat=${compat}&epoch=bad`, { headers: browserHeaders }), env)).status, 404);
  assert.equal(selected.length, beforeInvalid);
});
