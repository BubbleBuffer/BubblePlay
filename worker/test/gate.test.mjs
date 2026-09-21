import test from "node:test";
import assert from "node:assert/strict";
import { ApprovalGate, isInitialNavigation } from "../src/gate.js";
import worker from "../src/index.js";

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
  const held = f.gate.fetch(navigation(ip));
  await waitFor(() => f.notices.length > 0);
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
const DEBUG_COMPATIBILITY = "wss-v3-host20-edge12-bench2-hevc-tiles-bpip4-v15";

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

async function waitFor(predicate, timeoutMs = 1000) {
  const deadline = Date.now() + timeoutMs;
  while (!predicate()) {
    if (Date.now() >= deadline) throw new Error("condition did not become true");
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
}

test("recognizes only the configured initial document navigation", () => {
  assert.equal(isInitialNavigation(navigation()), true);
  assert.equal(isInitialNavigation(new Request("https://x.test/app.js", { headers: { Accept: "text/html" } })), false);
  assert.equal(isInitialNavigation(new Request("https://x.test/", { method: "POST", body: "x" })), false);
  assert.equal(isInitialNavigation(new Request("https://x.test/", { headers: { Accept: "text/html", "Sec-Fetch-Dest": "image" } })), false);
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

test("edge routing keeps host authorization separate and sends all browser HTTP through the gate", async () => {
  const forwarded = [];
  const gate = {
    async fetch(request) {
      forwarded.push(new URL(request.url).pathname);
      return new Response("gated", { status: 418 });
    },
  };
  const env = {
    HOST_GATE_TOKEN: "host-only",
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
  const compat = "wss-v3-host20-edge12-bench2-hevc-tiles-bpip4-v15";
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
  const compat = "wss-v3-host20-edge12-bench2-hevc-tiles-bpip4-v15";

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
});

test("media websocket routing uses an isolated Durable Object namespace", async () => {
  const selected = [];
  const mediaResponse = new Response("media", { status: 200 });
  const env = {
    HOST_GATE_TOKEN: "host-only",
    GATE: { idFromName: () => "gate", get: () => ({ fetch: async () => new Response(null, { status: 204 }) }) },
    ROOM: { idFromName: (name) => { selected.push(`signal:${name}`); return name; }, get: () => ({ fetch: async () => new Response("signal") }) },
    MEDIA_ROOM: { idFromName: (name) => { selected.push(`media:${name}`); return name; }, get: () => ({ fetch: async () => mediaResponse }) },
  };
  const response = await worker.fetch(new Request("https://playfull-cat.example/media-ws?stripe=2", {
    headers: { Upgrade: "websocket", Cookie: "remembered=1" },
  }), env);
  assert.equal(response, mediaResponse);
  assert.deepEqual(selected, ["media:media-v1"]);
});

test("lab websocket routing is isolated by canonical run id without changing production rooms", async () => {
  const selected = [];
  const signalResponse = new Response("signal", { status: 200 });
  const mediaResponse = new Response("media", { status: 200 });
  const env = {
    HOST_GATE_TOKEN: "host-only",
    GATE: { idFromName: () => "gate", get: () => ({ fetch: async () => new Response(null, { status: 204 }) }) },
    ROOM: { idFromName: (name) => { selected.push(`signal:${name}`); return name; }, get: () => ({ fetch: async () => signalResponse }) },
    MEDIA_ROOM: { idFromName: (name) => { selected.push(`media:${name}`); return name; }, get: () => ({ fetch: async () => mediaResponse }) },
  };
  const runA = "123e4567-e89b-42d3-a456-426614174000";
  const runB = "123e4567-e89b-42d3-a456-426614174001";
  const epoch = "0123456789abcdef0123456789abcdef";
  const compat = "wss-v3-host20-edge12-bench2-hevc-tiles-bpip4-v15";
  const browserHeaders = { Upgrade: "websocket", Cookie: "remembered=1" };
  const hostHeaders = { Upgrade: "websocket", Authorization: "Bearer host-only" };

  assert.equal(await worker.fetch(new Request(`https://playfull-cat.example/lab-ws?run=${runA}&compat=${compat}`, { headers: browserHeaders }), env), signalResponse);
  assert.equal(await worker.fetch(new Request(`https://playfull-cat.example/lab-host-ws?run=${runA}&compat=${compat}`, { headers: hostHeaders }), env), signalResponse);
  assert.equal(await worker.fetch(new Request(`https://playfull-cat.example/lab-media-ws?v=2&run=${runA}&compat=${compat}&epoch=${epoch}&stripe=2`, { headers: browserHeaders }), env), mediaResponse);
  assert.equal(await worker.fetch(new Request(`https://playfull-cat.example/lab-host-media-ws?v=2&run=${runA}&compat=${compat}&epoch=${epoch}&stripe=2`, { headers: hostHeaders }), env), mediaResponse);
  assert.equal(await worker.fetch(new Request(`https://playfull-cat.example/lab-ws?run=${runB}&compat=${compat}`, { headers: browserHeaders }), env), signalResponse);

  assert.deepEqual(selected, [
    `signal:lab-signal-v1:${runA}`,
    `signal:lab-signal-v1:${runA}`,
    `media:lab-media-v2:${runA}:${epoch}:2`,
    `media:lab-media-v2:${runA}:${epoch}:2`,
    `signal:lab-signal-v1:${runB}`,
  ]);

  const beforeInvalid = selected.length;
  assert.equal((await worker.fetch(new Request("https://playfull-cat.example/lab-ws?run=not-a-uuid", { headers: browserHeaders }), env)).status, 404);
  assert.equal((await worker.fetch(new Request(`https://playfull-cat.example/lab-media-ws?v=2&run=${runA}&epoch=bad&stripe=2`, { headers: browserHeaders }), env)).status, 404);
  assert.equal(selected.length, beforeInvalid);
});

test("media protocol v3 admits bounded lane ids without relaxing v2", async () => {
  const selected = [];
  const mediaResponse = new Response("media", { status: 200 });
  const env = {
    HOST_GATE_TOKEN: "host-only",
    GATE: { idFromName: () => "gate", get: () => ({ fetch: async () => new Response(null, { status: 204 }) }) },
    ROOM: { idFromName: name => name, get: () => ({ fetch: async () => new Response("signal") }) },
    MEDIA_ROOM: { idFromName: name => { selected.push(name); return name; }, get: () => ({ fetch: async () => mediaResponse }) },
  };
  const run = "123e4567-e89b-42d3-a456-426614174000";
  const epoch = "0123456789abcdef0123456789abcdef";
  const compat = "wss-v3-host20-edge12-bench2-hevc-tiles-bpip4-v15";
  const headers = { Upgrade: "websocket", Authorization: "Bearer host-only" };

  const lane15 = await worker.fetch(new Request(
    `https://playfull-cat.example/lab-host-media-ws?v=3&run=${run}&compat=${compat}&epoch=${epoch}&stripe=15`, { headers }), env);
  assert.equal(lane15, mediaResponse);
  const lane0 = await worker.fetch(new Request(
    `https://playfull-cat.example/lab-host-media-ws?v=3&run=${run}&compat=${compat}&epoch=${epoch}&stripe=0`, { headers }), env);
  assert.equal(lane0, mediaResponse);
  assert.deepEqual(selected, [
    `lab-media-v3:${run}:${epoch}:15`,
    `lab-media-v3:${run}:${epoch}:0`,
  ], "v3 lab lanes should preserve stripe-isolated media actors");

  for (const url of [
    `https://playfull-cat.example/lab-host-media-ws?v=3&run=${run}&compat=${compat}&epoch=${epoch}&stripe=16`,
    `https://playfull-cat.example/lab-host-media-ws?v=3&run=${run}&compat=${compat}&epoch=${epoch}&stripe=-1`,
    `https://playfull-cat.example/lab-host-media-ws?v=2&run=${run}&compat=${compat}&epoch=${epoch}&stripe=4`,
  ]) assert.equal((await worker.fetch(new Request(url, { headers }), env)).status, 404);
  assert.equal(selected.length, 2);
});
