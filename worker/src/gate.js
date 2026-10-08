import {
  CLIENT_DIAGNOSTIC_PATH,
  clientDiagnosticAttempt,
  ingestClientDiagnostic,
  readClientDiagnostic,
} from "./client_diag.js";
import { STREAM_LAB_COMPATIBILITY } from "../public/media.js";
import { approvalMethods } from "./approval-methods.js";
import { validApprovalMessage } from "./protocol-validation.js";

const COOKIE_NAME = "__Host-gate";
const SESSION_PREFIX = "session:";
const AUTH_COOKIE_NAME = "__Host-bp-session";
const AUTH_SESSION_PREFIX = "auth-session:";
const AUTH_RATE_PREFIX = "auth-rate:";
const SOCKET_ROOM_PREFIX = "socket-room:";
const DEFAULT_APPROVAL_TTL_SECONDS = 90;
const SESSION_TTL_MS = 30 * 24 * 60 * 60 * 1000;
const AUTH_REMEMBER_TTL_MS = 30 * 24 * 60 * 60 * 1000;
const AUTH_BROWSER_TTL_MS = 12 * 60 * 60 * 1000;
const AUTH_WINDOW_MS = 10 * 60 * 1000;
const AUTH_LOCKOUT_MS = 15 * 60 * 1000;
const AUTH_MAX_FAILURES = 5;
const MAX_PENDING = 10;
const LOGIN_STYLE_PATH = "/bubbleplay.css";
const FAVICON_PATHS = new Set(["/favicon.svg", "/favicon-32.png"]);
const DEFAULT_SOURCE_URL = "https://github.com/BubbleBuffer/BubblePlay";

export function sourceURL(value) {
  try {
    const url = new URL(value);
    if (["https:", "http:"].includes(url.protocol) && !url.username && !url.password) return url.href;
  } catch { /* unset or invalid: use the public source */ }
  return DEFAULT_SOURCE_URL;
}
const LOGIN_FONT_PATHS = new Set([
  "/fonts/atkinson-hyperlegible-next.woff2",
  "/fonts/bricolage-grotesque.woff2",
  "/fonts/fragment-mono.woff2",
]);

export function isInitialNavigation(request) {
  if (request.method !== "GET") return false;
  const url = new URL(request.url);
  if (url.pathname !== "/") return false;
  if (!request.headers.get("Accept")?.toLowerCase().includes("text/html")) return false;
  const mode = request.headers.get("Sec-Fetch-Mode");
  const dest = request.headers.get("Sec-Fetch-Dest");
  return (!mode || mode === "navigate") && (!dest || dest === "document");
}

export class ApprovalGate {
  constructor(state, env) {
    this.state = state;
    this.env = env;
    this.pendingById = new Map();
    this.pendingByCapability = new Map();
    this.pendingPrefixes = new Set();
    this.cooldownUntil = new Map();
    this.decisionWindows = new Map();
    this.locks = new Map();
    this.authorizationRevision = 0;
  }

  async fetch(request) {
    const url = new URL(request.url);
    const forceApproval = isInitialNavigation(request) && url.searchParams.get("approval") === "request";
    let methods;
    try { methods = approvalMethods(this.env); } catch { return empty(503); }
    if (url.pathname === "/__auth/methods") {
      if (request.method !== "GET") return hidden();
      return json({ totp: false, approval: methods, hostOnline: methods.includes("host") && await this.hostOnline(), sourceURL: sourceURL(this.env.SOURCE_URL) });
    }
    if (url.pathname === "/__approval/decision") return this.hostDecision(request);
    if (url.pathname === "/__auth/socket") return this.socketAuth(request);
    if (url.pathname === "/__auth/socket-register") return this.registerSocketRoom(request);
    if (url.pathname === "/__auth/socket-unregister") return this.registerSocketRoom(request, true);
    if (url.pathname === "/__gate/decision") return this.decide(request);
    if (url.pathname.startsWith("/__gate/manage/")) return this.manage(request, url);
    if (url.pathname === "/__gate/check") {
      return (await this.validSession(request)) ? empty(204) : hidden();
    }
    if (url.pathname === "/__auth/check") {
      return (await this.validSession(request)) && (await this.validAuthSession(request))
        ? empty(204)
        : hidden();
    }
    if (url.pathname === "/__auth/login") return this.login(request);
    if (url.pathname === "/__auth/logout") return this.endBrowserSession(request, false);
    if (url.pathname === "/__auth/forget-browser") return this.endBrowserSession(request, true);
    if (url.pathname === CLIENT_DIAGNOSTIC_PATH || clientDiagnosticAttempt(url.pathname))
      return this.clientDiagnostic(request, url);
    if ((FAVICON_PATHS.has(url.pathname) || methods.includes("host") && isLoginAsset(url.pathname)) && request.method === "GET") return this.serveAsset(request);

    if (!forceApproval && await this.validSession(request)) {
      if (await this.validAuthSession(request)) return this.serveAsset(request);
      if (isInitialNavigation(request)) return this.serveLogin(request);
      if (isLoginAsset(url.pathname)) return this.serveAsset(request);
      return hidden();
    }
    if (!isInitialNavigation(request)) return hidden();
    if (methods.length === 1 && methods[0] === "host" && !(await this.hostOnline())) {
      const login = await this.serveLogin(request);
      const body = (await login.text()).replace('<body class="auth-page">', '<body class="auth-page" data-approval-required>');
      return new Response(body, { status: 503, headers: login.headers });
    }
    if (this.pendingById.size >= MAX_PENDING) return hidden();

    const prefix = ipPrefix(request.headers.get("CF-Connecting-IP") || "unknown");
    if ((this.cooldownUntil.get(prefix) || 0) > Date.now()) return hidden();
    if (this.pendingPrefixes.has(prefix)) return hidden();
    return this.hold(request, prefix, methods);
  }

  async hold(request, prefix, methods = approvalMethods(this.env)) {
    const requestId = randomToken(16);
    const approveToken = randomToken(32);
    const denyToken = randomToken(32);
    const ttlSeconds = boundedNumber(this.env.GATE_APPROVAL_TTL_SECONDS, 30, 120, DEFAULT_APPROVAL_TTL_SECONDS);
    const createdAt = Date.now();
    const ttlMs = Number.isFinite(this.env.__gateApprovalTtlMs)
      ? Math.max(1, this.env.__gateApprovalTtlMs)
      : ttlSeconds * 1000;

    let resolveHeld;
    const held = new Promise((resolve) => { resolveHeld = resolve; });
    const pending = {
      id: requestId,
      prefix,
      request,
      createdAt,
      expiresAt: createdAt + ttlMs,
      approveHash: null,
      denyHash: null,
      resolve: resolveHeld,
      timer: null,
      abort: null,
      state: "pending",
      hostNotified: false,
    };
    // Take the slot and the prefix before the first await. Hashing yields to
    // other requests, and a burst that passed the checks in fetch() together
    // would otherwise register one prefix several times and exceed the cap.
    this.pendingById.set(requestId, pending);
    this.pendingPrefixes.add(prefix);

    pending.timer = setTimeout(() => this.finish(pending, "expired"), ttlMs);
    pending.abort = () => this.finish(pending, "disconnected");
    request.signal?.addEventListener("abort", pending.abort, { once: true });

    pending.approveHash = await hashToken(approveToken);
    pending.denyHash = await hashToken(denyToken);
    if (pending.state !== "pending") return held;
    this.pendingByCapability.set(pending.approveHash, { id: requestId, decision: "approve" });
    this.pendingByCapability.set(pending.denyHash, { id: requestId, decision: "deny" });

    // Dispatch all notifiers before awaiting either. A stalled publisher must
    // not delay a decision arriving through the host socket.
    const notifications = methods.map(method => method === "ntfy"
      ? this.publishNotice(pending, approveToken, denyToken)
      : this.publishHost(pending));
    void Promise.allSettled(notifications).then(results => {
      if (results.every(result => result.status === "rejected")) this.finish(pending, "notification-failed");
    });
    return held;
  }

  hostRoom() {
    return this.env.ROOM?.get(this.env.ROOM.idFromName("room-v3"));
  }

  async hostOnline() {
    try {
      const response = await this.hostRoom()?.fetch(new Request("https://room.internal/__approval/status"));
      return response?.ok === true && (await response.json()).hostOnline === true;
    } catch { return false; }
  }

  async publishHost(pending) {
    const room = this.hostRoom();
    if (!room) throw new Error("host is offline");
    pending.hostNotified = true;
    const response = await room.fetch(new Request("https://room.internal/__approval/request", {
      method: "POST", headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ type: "approval-request", id: pending.id,
        client: approvalClient(pending.request), agent: approvalAgent(pending.request), expiresAt: pending.expiresAt }),
    }));
    if (response.status !== 204) throw new Error("host is offline");
    // A notifier can accept after the other notifier already made a decision.
    if (pending.state !== "pending") this.cancelHost(pending);
  }

  cancelHost(pending) {
    if (!pending.hostNotified) return;
    try {
      const operation = this.hostRoom()?.fetch(new Request("https://room.internal/__approval/cancel", {
        method: "POST", headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ type: "approval-cancel", id: pending.id }),
      }));
      if (operation) this.background(operation.catch(() => {}));
    } catch { /* Cancellation is best effort; the held request still resolves. */ }
  }

  background(operation) {
    if (this.state.waitUntil) this.state.waitUntil(operation);
    else void operation;
  }

  async hostDecision(request) {
    if (request.method !== "POST" || new URL(request.url).hostname !== "gate.internal") return hidden();
    let decision;
    try { decision = await request.json(); } catch { return hidden(); }
    if (decision?.type !== "approval-decision" || !validApprovalMessage(decision)) return hidden();
    const pending = this.pendingById.get(decision.id);
    if (!pending?.hostNotified || pending.state !== "pending") return empty(204);
    if (pending.expiresAt <= Date.now()) { this.finish(pending, "expired"); return empty(204); }
    return this.completeDecision(pending, decision.approve);
  }

  async publishNotice(pending, approveToken, denyToken) {
    const base = String(this.env.NTFY_URL || "").replace(/\/$/, "");
    const topic = String(this.env.NTFY_TOPIC || "");
    const token = String(this.env.NTFY_TOKEN || "");
    if (!base || !topic || !token) throw new Error("ntfy is not configured");
    const decisionUrl = new URL("/__gate/decision", pending.request.url).toString();
    const headers = { Authorization: `Bearer ${token}`, "Content-Type": "application/json" };
    const body = {
      topic,
      title: "BubblePlay access request",
      message: requestSummary(pending.request),
      tags: ["lock"],
      actions: [
        { action: "http", label: "Approve", url: decisionUrl, method: "POST", headers: { Authorization: `Bearer ${approveToken}` }, body: "approve", clear: true },
        { action: "http", label: "Deny", url: decisionUrl, method: "POST", headers: { Authorization: `Bearer ${denyToken}` }, body: "deny", clear: true },
      ],
    };
    const fetcher = this.env.__gateFetch || fetch;
    const response = await fetcher(base, { method: "POST", headers, body: JSON.stringify(body) });
    if (!response.ok) throw new Error(`ntfy returned ${response.status}`);
    console.log(`[gate] pending request=${shortId(pending.id)}`);
  }

  async decide(request) {
    if (request.method !== "POST") return hidden();
    if (!this.allowDecision(request)) return empty(204);
    const token = bearerToken(request);
    if (!token || token.length > 256) return empty(204);
    const capability = this.pendingByCapability.get(await hashToken(token));
    const pending = capability && this.pendingById.get(capability.id);
    if (!pending || pending.state !== "pending") return empty(204);
    const requested = (await request.text()).trim().toLowerCase();
    if (requested !== capability.decision) return empty(204);
    if (pending.state !== "pending") return empty(204);
    if (pending.expiresAt <= Date.now()) { this.finish(pending, "expired"); return empty(204); }
    return this.completeDecision(pending, capability.decision === "approve");
  }

  async completeDecision(pending, approve) {
    if (pending.state !== "pending") return empty(204);
    if (!approve) {
      this.finish(pending, "denied");
      return empty(204);
    }

    pending.state = "approved";
    this.detach(pending);
    try {
      const upstream = await this.serveLogin(pending.request);
      const sessionToken = randomToken(32);
      const sessionHash = await hashToken(sessionToken);
      const now = Date.now();
      await this.state.storage.put(`${SESSION_PREFIX}${sessionHash}`, {
        id: randomToken(12), createdAt: now, lastSeenAt: now,
        expiresAt: now + AUTH_BROWSER_TTL_MS, remembered: false,
      });
      const headers = new Headers(upstream.headers);
      headers.set("Set-Cookie", gateCookie(sessionToken, false));
      headers.set("Cache-Control", "no-store");
      pending.state = "consumed";
      pending.resolve(new Response(upstream.body, { status: upstream.status, statusText: upstream.statusText, headers }));
      console.log(`[gate] approved request=${shortId(pending.id)}`);
    } catch (error) {
      pending.state = "failed";
      pending.resolve(hidden());
      console.log(`[gate] release failed request=${shortId(pending.id)} error=${safeError(error)}`);
    }
    return empty(204);
  }

  finish(pending, state) {
    if (!pending || pending.state !== "pending") return false;
    pending.state = state;
    if (state === "denied" || state === "expired" || state === "notification-failed") {
      this.cooldownUntil.set(pending.prefix, Date.now() + 15_000);
    }
    this.detach(pending);
    pending.resolve(hidden());
    console.log(`[gate] ${state} request=${shortId(pending.id)}`);
    return true;
  }

  allowDecision(request) {
    const prefix = ipPrefix(request.headers.get("CF-Connecting-IP") || "unknown");
    const now = Date.now();
    const current = this.decisionWindows.get(prefix);
    if (!current || now - current.startedAt >= 60_000) {
      this.decisionWindows.set(prefix, { startedAt: now, count: 1 });
      return true;
    }
    current.count += 1;
    return current.count <= 60;
  }

  detach(pending) {
    this.cancelHost(pending);
    clearTimeout(pending.timer);
    pending.request.signal?.removeEventListener("abort", pending.abort);
    this.pendingById.delete(pending.id);
    this.pendingByCapability.delete(pending.approveHash);
    this.pendingByCapability.delete(pending.denyHash);
    this.pendingPrefixes.delete(pending.prefix);
  }

  async validSession(request) {
    const token = cookieValue(request.headers.get("Cookie"), COOKIE_NAME);
    if (!token || token.length > 256) return false;
    const key = `${SESSION_PREFIX}${await hashToken(token)}`;
    const session = await this.state.storage.get(key);
    if (!session || session.revokedAt || session.expiresAt <= Date.now()) {
      if (session?.expiresAt <= Date.now()) await this.state.storage.delete(key);
      return false;
    }
    const now = Date.now();
    if (session.remembered && now - (session.lastSeenAt || 0) >= 24 * 60 * 60 * 1000) {
      session.expiresAt = now + SESSION_TTL_MS;
      session.lastSeenAt = now;
      await this.state.storage.put(key, session);
    }
    return session;
  }

  async login(request) {
    if (request.method !== "POST" || !(await this.validSession(request))) return hidden();
    const url = new URL(request.url);
    if (request.headers.get("Origin") !== url.origin || !request.headers.get("Content-Type")?.toLowerCase().startsWith("application/json")) {
      return hidden();
    }
    const prefix = ipPrefix(request.headers.get("CF-Connecting-IP") || "unknown");
    const rateKey = `${AUTH_RATE_PREFIX}${await hashToken(prefix)}`;
    const now = Date.now();
    // Count an attempt before reading its body or hashing its password. The
    // per-prefix lock prevents concurrent reads from overwriting failures.
    const rate = await this.withLock(rateKey, async () => {
      let current = await this.state.storage.get(rateKey);
      if (current?.lockedUntil > Date.now()) return { blocked: true, ...current };
      if (!current || Date.now() - current.startedAt >= AUTH_WINDOW_MS)
        current = { startedAt: Date.now(), failures: 0, lockedUntil: 0 };
      current.failures += 1;
      current.inFlight = (current.inFlight || 0) + 1;
      current.revision = (current.revision || 0) + 1;
      if (current.failures >= AUTH_MAX_FAILURES) current.lockedUntil = Date.now() + AUTH_LOCKOUT_MS;
      await this.state.storage.put(rateKey, current);
      return { ...current };
    });
    if (rate.blocked) return retryLater(rate.lockedUntil - Date.now());

    let payload;
    try {
      const raw = await request.text();
      if (raw.length > 1_024) {
        await this.settlePasswordAttempt(rateKey, rate, false);
        return rate.lockedUntil > now ? retryLater(rate.lockedUntil - now) : authFailed();
      }
      payload = JSON.parse(raw);
    } catch {
      await this.settlePasswordAttempt(rateKey, rate, false);
      return rate.lockedUntil > now ? retryLater(rate.lockedUntil - now) : authFailed();
    }
    const password = typeof payload?.password === "string" ? payload.password : "";
    const configured = typeof this.env.APP_PASSWORD === "string" ? this.env.APP_PASSWORD : "";
    const passwordOk = password.length > 0 && password.length <= 256 && configured.length > 0 &&
      (await hashToken(password)) === (await hashToken(configured));
    if (!passwordOk) {
      await this.settlePasswordAttempt(rateKey, rate, false);
      return rate.lockedUntil > now ? retryLater(rate.lockedUntil - now) : authFailed();
    }

    await this.settlePasswordAttempt(rateKey, rate, true);
    const remember = payload.remember === true;
    const ttlMs = remember ? AUTH_REMEMBER_TTL_MS : AUTH_BROWSER_TTL_MS;
    const token = randomToken(32);
    const tokenHash = await hashToken(token);
    const passwordTag = await passwordFingerprint(configured, this.env.GATE_ADMIN_TOKEN);
    const gateToken = cookieValue(request.headers.get("Cookie"), COOKIE_NAME);
    const gateKey = gateToken ? `${SESSION_PREFIX}${await hashToken(gateToken)}` : null;
    if (gateKey) {
      const gateSession = await this.state.storage.get(gateKey);
      if (gateSession) {
        gateSession.remembered = remember;
        gateSession.expiresAt = now + ttlMs;
        gateSession.lastSeenAt = now;
        await this.state.storage.put(gateKey, gateSession);
      }
    }
    await this.state.storage.put(`${AUTH_SESSION_PREFIX}${tokenHash}`, {
      id: randomToken(12), createdAt: now, lastSeenAt: now,
      expiresAt: now + ttlMs, remembered: remember, passwordTag,
      gateHash: gateKey?.slice(SESSION_PREFIX.length),
    });
    const suffix = remember ? `; Max-Age=${Math.floor(ttlMs / 1000)}` : "";
    const responseHeaders = new Headers({
      "Cache-Control": "no-store",
      "Set-Cookie": `${AUTH_COOKIE_NAME}=${token}; Path=/; Secure; HttpOnly; SameSite=Lax${suffix}`,
    });
    if (gateToken) responseHeaders.append("Set-Cookie", gateCookie(gateToken, remember));
    return new Response(null, {
      status: 204,
      headers: responseHeaders,
    });
  }

  async validAuthSession(request) {
    const token = cookieValue(request.headers.get("Cookie"), AUTH_COOKIE_NAME);
    const gateToken = cookieValue(request.headers.get("Cookie"), COOKIE_NAME);
    if (!token || token.length > 256 || !gateToken || gateToken.length > 256) return false;
    const gateHash = await hashToken(gateToken);
    const authHash = await hashToken(token);
    return !!(await this.validateIdentity({ gateHash, authHash }));
  }

  async settlePasswordAttempt(rateKey, reserved, success) {
    await this.withLock(rateKey, async () => {
      const current = await this.state.storage.get(rateKey);
      if (!current || current.startedAt !== reserved.startedAt) return;
      const otherInFlight = Math.max(0, (current.inFlight || 1) - 1);
      if (success && otherInFlight === 0 && current.revision === reserved.revision) {
        await this.state.storage.delete(rateKey);
      } else {
        current.inFlight = otherInFlight;
        if (success) current.failures = Math.max(0, current.failures - 1);
        // A burst can provisionally consume the entire budget even when its
        // passwords are correct. Once every reservation settles, retain a
        // lockout only when the actual failures still reach the threshold.
        if (otherInFlight === 0 && current.failures < AUTH_MAX_FAILURES) current.lockedUntil = 0;
        await this.state.storage.put(rateKey, current);
      }
    });
  }

  async withLock(key, operation) {
    const previous = this.locks.get(key) || Promise.resolve();
    const running = previous.catch(() => {}).then(operation);
    this.locks.set(key, running);
    try { return await running; }
    finally { if (this.locks.get(key) === running) this.locks.delete(key); }
  }

  async validateIdentity(identity) {
    if (!validIdentity(identity)) return false;
    return this.withLock(`${AUTH_SESSION_PREFIX}${identity.authHash}`, () => this.validateIdentityLocked(identity));
  }

  async validateIdentityLocked({ gateHash, authHash }) {
    const gate = await this.state.storage.get(`${SESSION_PREFIX}${gateHash}`);
    if (!gate || gate.revokedAt || gate.expiresAt <= Date.now()) return false;
    const key = `${AUTH_SESSION_PREFIX}${authHash}`;
    const session = await this.state.storage.get(key);
    if (!session || session.revokedAt || session.expiresAt <= Date.now()) {
      if (session) await this.state.storage.delete(key);
      return false;
    }
    const configured = typeof this.env.APP_PASSWORD === "string" ? this.env.APP_PASSWORD : "";
    const currentTag = await passwordFingerprint(configured, this.env.GATE_ADMIN_TOKEN);
    if (!configured || !session.passwordTag || session.passwordTag !== currentTag) {
      this.authorizationRevision += 1;
      await this.state.storage.delete(key);
      this.authorizationRevision += 1;
      if (this.notifiedPasswordTag !== currentTag) {
        this.notifiedPasswordTag = currentTag;
        this.background(this.notifySocketRooms());
      }
      return false;
    }
    if (session.gateHash && session.gateHash !== gateHash) return false;
    if (!session.gateHash) {
      session.gateHash = gateHash;
      await this.state.storage.put(key, session);
    }
    const now = Date.now();
    if (session.remembered && now - session.lastSeenAt >= 24 * 60 * 60 * 1000) {
      session.expiresAt = now + AUTH_REMEMBER_TTL_MS;
      session.lastSeenAt = now;
      await this.state.storage.put(key, session);
    } else if (now - session.lastSeenAt >= 5 * 60 * 1000) {
      session.lastSeenAt = now;
      await this.state.storage.put(key, session);
    }
    return { gateHash, authHash, expiresAt: Math.min(gate.expiresAt, session.expiresAt) };
  }

  async socketAuth(request) {
    if (new URL(request.url).hostname !== "gate.internal") return hidden();
    if (request.method === "GET") {
      const gateToken = cookieValue(request.headers.get("Cookie"), COOKIE_NAME);
      const authToken = cookieValue(request.headers.get("Cookie"), AUTH_COOKIE_NAME);
      if (!gateToken || !authToken || gateToken.length > 256 || authToken.length > 256) return hidden();
      const identity = await this.validateIdentity({ gateHash: await hashToken(gateToken), authHash: await hashToken(authToken) });
      return identity ? json(identity) : hidden();
    }
    if (request.method !== "POST") return hidden();
    let identity;
    try { identity = await request.json(); } catch { return hidden(); }
    return await this.validateIdentity(identity) ? empty(204) : hidden();
  }

  async registerSocketRoom(request, unregister = false) {
    if (request.method !== "POST" || new URL(request.url).hostname !== "gate.internal") return hidden();
    let body;
    try { body = await request.json(); } catch { return hidden(); }
    if (!validIdentity(body?.identity) || !["ROOM", "MEDIA_ROOM"].includes(body.namespace) ||
      typeof body.roomName !== "string" || !/^[A-Za-z0-9:_-]{1,180}$/.test(body.roomName)) return hidden();
    const key = `${SOCKET_ROOM_PREFIX}${body.namespace}:${body.roomName}:${body.identity.authHash}`;
    if (unregister) { await this.state.storage.delete(key); return empty(204); }
    const revision = this.authorizationRevision;
    const identity = await this.validateIdentity(body.identity);
    if (!identity) return hidden();
    await this.withLock(SOCKET_ROOM_PREFIX, async () => {
      const rooms = await this.state.storage.list({ prefix: SOCKET_ROOM_PREFIX });
      for (const [storedKey, room] of rooms) {
        if (room.expiresAt <= Date.now()) { await this.state.storage.delete(storedKey); rooms.delete(storedKey); }
      }
      if (!rooms.has(key) && rooms.size >= 2048) return;
      await this.state.storage.put(key, { namespace: body.namespace, roomName: body.roomName, ...identity });
    });
    // If revocation raced the subscription write, recheck the new Room too.
    // Otherwise the earlier broadcast could have missed this registration.
    if (revision !== this.authorizationRevision) await this.notifySocketRooms();
    return await this.state.storage.get(key) ? empty(204) : empty(503);
  }

  async notifySocketRooms() {
    const rooms = await this.state.storage.list({ prefix: SOCKET_ROOM_PREFIX });
    const unique = new Map();
    for (const [key, room] of rooms) {
      if (room.expiresAt <= Date.now()) { await this.state.storage.delete(key); continue; }
      unique.set(`${room.namespace}:${room.roomName}`, room);
    }
    await Promise.allSettled([...unique.values()].map(async room => {
      const namespace = this.env[room.namespace];
      if (!namespace) return Promise.resolve();
      return namespace.get(namespace.idFromName(room.roomName)).fetch(new Request("https://room.internal/__auth/recheck", { method: "POST" }));
    }));
  }

  async endBrowserSession(request, forgetGate) {
    if (request.method !== "POST" || !sameOriginRequest(request)) return hidden();
    if (!(await this.validSession(request)) || !(await this.validAuthSession(request))) return hidden();

    const authToken = cookieValue(request.headers.get("Cookie"), AUTH_COOKIE_NAME);
    const gateToken = cookieValue(request.headers.get("Cookie"), COOKIE_NAME);
    this.authorizationRevision += 1;
    await this.state.storage.delete(`${AUTH_SESSION_PREFIX}${await hashToken(authToken)}`);
    if (forgetGate) await this.state.storage.delete(`${SESSION_PREFIX}${await hashToken(gateToken)}`);
    this.authorizationRevision += 1;
    await this.notifySocketRooms();

    const headers = new Headers({ "Cache-Control": "no-store" });
    headers.append("Set-Cookie", expiredCookie(AUTH_COOKIE_NAME));
    if (forgetGate) headers.append("Set-Cookie", expiredCookie(COOKIE_NAME));
    return new Response(null, { status: 204, headers });
  }

  async clientDiagnostic(request, url) {
    if (!(await this.validSession(request)) || !(await this.validAuthSession(request))) return hidden();
    const attemptId = clientDiagnosticAttempt(url.pathname);
    if (request.method === "GET" && attemptId) {
      const timeline = await readClientDiagnostic(this.state.storage, attemptId);
      return timeline ? json(timeline) : hidden();
    }
    if (request.method !== "POST" || url.pathname !== CLIENT_DIAGNOSTIC_PATH ||
        !sameOriginRequest(request) ||
        !request.headers.get("Content-Type")?.toLowerCase().startsWith("application/json")) return hidden();
    const result = await ingestClientDiagnostic(
      this.state.storage,
      await request.text(),
      STREAM_LAB_COMPATIBILITY,
    );
    if (!result.ok) return empty(result.status);
    console.log(`[client-diag] ${JSON.stringify(result.event)}`);
    return empty(204);
  }

  async serveAsset(request) {
    const response = await this.env.ASSETS.fetch(request);
    const headers = new Headers(response.headers);
    const path = new URL(request.url).pathname;
    headers.set("Cache-Control", LOGIN_FONT_PATHS.has(path) ? "public, max-age=31536000, immutable" : "no-store");
    return new Response(response.body, { status: response.status, statusText: response.statusText, headers });
  }

  async serveLogin(request) {
    const url = new URL(request.url);
    url.pathname = "/login.html";
    url.search = "";
    const loginRequest = new Request(url, { method: "GET", headers: { Accept: "text/html" } });
    const response = await this.serveAsset(loginRequest);
    const headers = new Headers(response.headers);
    headers.set("Content-Security-Policy", "default-src 'none'; img-src 'self'; style-src 'self'; font-src 'self'; script-src 'unsafe-inline'; connect-src 'self'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'");
    headers.set("X-Frame-Options", "DENY");
    headers.set("X-Content-Type-Options", "nosniff");
    return new Response(response.body, { status: response.status, statusText: response.statusText, headers });
  }

  async manage(request, url) {
    if (!(await authorized(request, this.env.GATE_ADMIN_TOKEN))) return hidden();
    if (request.method === "GET" && url.pathname === "/__gate/manage/sessions") {
      const stored = await this.state.storage.list({ prefix: SESSION_PREFIX });
      const sessions = [...stored.values()].map(({ id, createdAt, lastSeenAt, expiresAt, revokedAt }) => ({ id, createdAt, lastSeenAt, expiresAt, revokedAt }));
      return json(sessions);
    }
    if (request.method === "GET" && url.pathname === "/__gate/manage/auth-sessions") {
      const stored = await this.state.storage.list({ prefix: AUTH_SESSION_PREFIX });
      return json([...stored.values()].map(({ id, createdAt, lastSeenAt, expiresAt, remembered }) => ({ id, createdAt, lastSeenAt, expiresAt, remembered })));
    }
    if (request.method === "DELETE" && url.pathname === "/__gate/manage/auth-sessions") {
      this.authorizationRevision += 1;
      const stored = await this.state.storage.list({ prefix: AUTH_SESSION_PREFIX });
      await Promise.all([...stored.keys()].map((key) => this.state.storage.delete(key)));
      this.authorizationRevision += 1;
      await this.notifySocketRooms();
      return empty(204);
    }
    if (request.method === "DELETE" && url.pathname === "/__gate/manage/sessions") {
      this.authorizationRevision += 1;
      const stored = await this.state.storage.list({ prefix: SESSION_PREFIX });
      await Promise.all([...stored.keys()].map((key) => this.state.storage.delete(key)));
      this.authorizationRevision += 1;
      await this.notifySocketRooms();
      return empty(204);
    }
    const match = url.pathname.match(/^\/__gate\/manage\/sessions\/([A-Za-z0-9_-]+)$/);
    if (request.method === "DELETE" && match) {
      this.authorizationRevision += 1;
      const stored = await this.state.storage.list({ prefix: SESSION_PREFIX });
      const found = [...stored].find(([, session]) => session.id === match[1]);
      if (found) await this.state.storage.delete(found[0]);
      this.authorizationRevision += 1;
      if (found) await this.notifySocketRooms();
      return empty(204);
    }
    return hidden();
  }
}

export async function authorized(request, configuredToken) {
  const supplied = bearerToken(request);
  if (!supplied || typeof configuredToken !== "string" || !configuredToken) return false;
  return (await hashToken(supplied)) === (await hashToken(configuredToken));
}

function bearerToken(request) {
  const value = request.headers.get("Authorization") || "";
  return value.startsWith("Bearer ") ? value.slice(7) : "";
}

function cookieValue(header, name) {
  if (!header) return "";
  for (const part of header.split(";")) {
    const [key, ...value] = part.trim().split("=");
    if (key === name) return value.join("=");
  }
  return "";
}

async function hashToken(token) {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(token));
  return [...new Uint8Array(digest)].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}

async function passwordFingerprint(password, adminToken) {
  if (!password) return "";
  if (!adminToken) return hashToken(password);
  const key = await crypto.subtle.importKey(
    "raw", new TextEncoder().encode(adminToken), { name: "HMAC", hash: "SHA-256" }, false, ["sign"],
  );
  const digest = await crypto.subtle.sign("HMAC", key, new TextEncoder().encode(password));
  return [...new Uint8Array(digest)].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}

function randomToken(bytes) {
  const value = new Uint8Array(bytes);
  crypto.getRandomValues(value);
  return btoa(String.fromCharCode(...value)).replaceAll("+", "-").replaceAll("/", "_").replace(/=+$/, "");
}

function ipPrefix(ip) {
  if (ip.includes(".")) return ip.split(".").slice(0, 3).join(".");
  if (ip.includes(":")) return ip.split(":").slice(0, 4).join(":");
  return "unknown";
}

function requestSummary(request) {
  const url = new URL(request.url);
  const ip = request.headers.get("CF-Connecting-IP") || "unknown";
  const ua = (request.headers.get("User-Agent") || "unknown").slice(0, 160);
  return `${request.method} ${url.host}${url.pathname}\nIP: ${ip}\n${ua}`;
}

function validIdentity(value) {
  return value !== null && typeof value === "object" &&
    typeof value.gateHash === "string" && /^[a-f0-9]{64}$/.test(value.gateHash) &&
    typeof value.authHash === "string" && /^[a-f0-9]{64}$/.test(value.authHash);
}

function approvalClient(request) {
  const ip = request.headers.get("CF-Connecting-IP") || "unknown";
  if (/^\d{1,3}(?:\.\d{1,3}){3}$/.test(ip) && ip.split(".").every(part => Number(part) <= 255))
    return `${ip.split(".").slice(0, 3).join(".")}.0/24`;
  // Expand before masking: text before the fourth colon is not necessarily
  // the first 64 bits of a compressed address. Embedded IPv4 is omitted.
  if (!/^[A-Fa-f0-9:]+$/.test(ip)) return "unknown";
  const halves = ip.split("::");
  if (halves.length > 2) return "unknown";
  const left = halves[0] ? halves[0].split(":") : [];
  const right = halves.length === 2 && halves[1] ? halves[1].split(":") : [];
  if (![...left, ...right].every(part => /^[A-Fa-f0-9]{1,4}$/.test(part))) return "unknown";
  const missing = 8 - left.length - right.length;
  if (halves.length === 1 ? missing !== 0 : missing < 1) return "unknown";
  const groups = [...left, ...Array(missing).fill("0"), ...right]
    .slice(0, 4).map(part => Number.parseInt(part, 16).toString(16));
  // The masked suffix is always zero. Fold preceding zero groups into the
  // same trailing compression to render a valid canonical /64 prefix.
  while (groups.at(-1) === "0") groups.pop();
  return `${groups.join(":")}::/64`;
}

function approvalAgent(request) {
  const ua = request.headers.get("User-Agent") || "Unknown browser";
  const browser = /(?:Edg|Chrome|Firefox|Version)\/([\d.]+)/.exec(ua);
  const name = ua.includes("Edg/") ? "Edge" : ua.includes("Chrome/") ? "Chrome" :
    ua.includes("Firefox/") ? "Firefox" : ua.includes("Safari/") ? "Safari" : "Browser";
  const os = ua.includes("Windows") ? "Windows" : ua.includes("Android") ? "Android" :
    /iPhone|iPad/.test(ua) ? "iOS" : ua.includes("Mac") ? "macOS" : ua.includes("Linux") ? "Linux" : "unknown OS";
  return `${name}${browser ? ` ${browser[1].split(".")[0]}` : ""} on ${os}`.replace(/[^\x20-\x7e]/g, "").slice(0, 80);
}

function isLoginAsset(pathname) {
  return FAVICON_PATHS.has(pathname) || pathname === LOGIN_STYLE_PATH || LOGIN_FONT_PATHS.has(pathname);
}

function sameOriginRequest(request) {
  return request.headers.get("Origin") === new URL(request.url).origin;
}

function boundedNumber(value, min, max, fallback) {
  const parsed = Number(value);
  return Number.isFinite(parsed) ? Math.min(max, Math.max(min, parsed)) : fallback;
}

function shortId(value) { return String(value).slice(0, 8); }
function safeError(error) { return String(error?.message || error).slice(0, 160); }
function gateCookie(token, remember) {
  const suffix = remember ? `; Max-Age=${Math.floor(SESSION_TTL_MS / 1000)}` : "";
  return `${COOKIE_NAME}=${token}; Path=/; Secure; HttpOnly; SameSite=Lax${suffix}`;
}
function expiredCookie(name) { return `${name}=; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age=0`; }
function empty(status) { return new Response(null, { status, headers: { "Cache-Control": "no-store" } }); }
function hidden() { return new Response("Not found", { status: 404, headers: { "Cache-Control": "no-store", "Content-Type": "text/plain; charset=utf-8" } }); }
function json(value) { return new Response(JSON.stringify(value), { headers: { "Cache-Control": "no-store", "Content-Type": "application/json" } }); }
function authFailed() { return new Response(JSON.stringify({ error: "Invalid password" }), { status: 401, headers: { "Cache-Control": "no-store", "Content-Type": "application/json" } }); }
function retryLater(delayMs) { return new Response(JSON.stringify({ error: "Try again later" }), { status: 429, headers: { "Cache-Control": "no-store", "Content-Type": "application/json", "Retry-After": String(Math.max(1, Math.ceil(delayMs / 1000))) } }); }
