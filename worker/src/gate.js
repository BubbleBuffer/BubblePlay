import {
  CLIENT_DIAGNOSTIC_PATH,
  clientDiagnosticAttempt,
  ingestClientDiagnostic,
  readClientDiagnostic,
} from "./client_diag.js";
import { STREAM_LAB_COMPATIBILITY } from "../public/media.js";

const COOKIE_NAME = "__Host-gate";
const SESSION_PREFIX = "session:";
const AUTH_COOKIE_NAME = "__Host-bp-session";
const AUTH_SESSION_PREFIX = "auth-session:";
const AUTH_RATE_PREFIX = "auth-rate:";
const DEFAULT_APPROVAL_TTL_SECONDS = 90;
const SESSION_TTL_MS = 30 * 24 * 60 * 60 * 1000;
const AUTH_REMEMBER_TTL_MS = 30 * 24 * 60 * 60 * 1000;
const AUTH_BROWSER_TTL_MS = 12 * 60 * 60 * 1000;
const AUTH_WINDOW_MS = 10 * 60 * 1000;
const AUTH_LOCKOUT_MS = 15 * 60 * 1000;
const AUTH_MAX_FAILURES = 5;
const MAX_PENDING = 10;
const LOGIN_STYLE_PATH = "/bubbleplay.css";
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
  }

  async fetch(request) {
    const url = new URL(request.url);
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

    if (await this.validSession(request)) {
      if (await this.validAuthSession(request)) return this.serveAsset(request);
      if (isInitialNavigation(request)) return this.serveLogin(request);
      if (isLoginAsset(url.pathname)) return this.serveAsset(request);
      return hidden();
    }
    if (!isInitialNavigation(request)) return hidden();
    if (this.pendingById.size >= MAX_PENDING) return hidden();

    const prefix = ipPrefix(request.headers.get("CF-Connecting-IP") || "unknown");
    if ((this.cooldownUntil.get(prefix) || 0) > Date.now()) return hidden();
    if (this.pendingPrefixes.has(prefix)) return hidden();
    return this.hold(request, prefix);
  }

  async hold(request, prefix) {
    const requestId = randomToken(16);
    const approveToken = randomToken(32);
    const denyToken = randomToken(32);
    const approveHash = await hashToken(approveToken);
    const denyHash = await hashToken(denyToken);
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
      approveHash,
      denyHash,
      resolve: resolveHeld,
      timer: null,
      abort: null,
      state: "pending",
    };
    this.pendingById.set(requestId, pending);
    this.pendingByCapability.set(approveHash, { id: requestId, decision: "approve" });
    this.pendingByCapability.set(denyHash, { id: requestId, decision: "deny" });
    this.pendingPrefixes.add(prefix);

    pending.timer = setTimeout(() => this.finish(pending, "expired"), ttlMs);
    pending.abort = () => this.finish(pending, "disconnected");
    request.signal?.addEventListener("abort", pending.abort, { once: true });

    try {
      await this.publishNotice(pending, approveToken, denyToken);
    } catch (error) {
      console.log(`[gate] notification failed request=${shortId(requestId)} error=${safeError(error)}`);
      this.finish(pending, "notification-failed");
    }
    return held;
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

    if (capability.decision === "deny") {
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
    let rate = await this.state.storage.get(rateKey);
    if (rate?.lockedUntil > now) return retryLater(rate.lockedUntil - now);
    if (!rate || now - rate.startedAt >= AUTH_WINDOW_MS) rate = { startedAt: now, failures: 0, lockedUntil: 0 };

    let payload;
    try {
      const raw = await request.text();
      if (raw.length > 1_024) return authFailed();
      payload = JSON.parse(raw);
    } catch {
      return authFailed();
    }
    const password = typeof payload?.password === "string" ? payload.password : "";
    const configured = typeof this.env.APP_PASSWORD === "string" ? this.env.APP_PASSWORD : "";
    const passwordOk = password.length > 0 && password.length <= 256 && configured.length > 0 &&
      (await hashToken(password)) === (await hashToken(configured));
    if (!passwordOk) {
      rate.failures += 1;
      if (rate.failures >= AUTH_MAX_FAILURES) rate.lockedUntil = now + AUTH_LOCKOUT_MS;
      await this.state.storage.put(rateKey, rate);
      return rate.lockedUntil > now ? retryLater(rate.lockedUntil - now) : authFailed();
    }

    await this.state.storage.delete(rateKey);
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
    if (!token || token.length > 256) return false;
    const key = `${AUTH_SESSION_PREFIX}${await hashToken(token)}`;
    const session = await this.state.storage.get(key);
    if (!session || session.expiresAt <= Date.now()) {
      if (session) await this.state.storage.delete(key);
      return false;
    }
    const configured = typeof this.env.APP_PASSWORD === "string" ? this.env.APP_PASSWORD : "";
    const currentTag = await passwordFingerprint(configured, this.env.GATE_ADMIN_TOKEN);
    if (!configured || !session.passwordTag || session.passwordTag !== currentTag) {
      await this.state.storage.delete(key);
      return false;
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
    return true;
  }

  async endBrowserSession(request, forgetGate) {
    if (request.method !== "POST" || !sameOriginRequest(request)) return hidden();
    if (!(await this.validSession(request)) || !(await this.validAuthSession(request))) return hidden();

    const authToken = cookieValue(request.headers.get("Cookie"), AUTH_COOKIE_NAME);
    const gateToken = cookieValue(request.headers.get("Cookie"), COOKIE_NAME);
    await this.state.storage.delete(`${AUTH_SESSION_PREFIX}${await hashToken(authToken)}`);
    if (forgetGate) await this.state.storage.delete(`${SESSION_PREFIX}${await hashToken(gateToken)}`);

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
    headers.set("Content-Security-Policy", "default-src 'none'; style-src 'self'; font-src 'self'; script-src 'unsafe-inline'; connect-src 'self'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'");
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
      const stored = await this.state.storage.list({ prefix: AUTH_SESSION_PREFIX });
      await Promise.all([...stored.keys()].map((key) => this.state.storage.delete(key)));
      return empty(204);
    }
    if (request.method === "DELETE" && url.pathname === "/__gate/manage/sessions") {
      const stored = await this.state.storage.list({ prefix: SESSION_PREFIX });
      await Promise.all([...stored.keys()].map((key) => this.state.storage.delete(key)));
      return empty(204);
    }
    const match = url.pathname.match(/^\/__gate\/manage\/sessions\/([A-Za-z0-9_-]+)$/);
    if (request.method === "DELETE" && match) {
      const stored = await this.state.storage.list({ prefix: SESSION_PREFIX });
      const found = [...stored].find(([, session]) => session.id === match[1]);
      if (found) await this.state.storage.delete(found[0]);
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

function isLoginAsset(pathname) {
  return pathname === LOGIN_STYLE_PATH || LOGIN_FONT_PATHS.has(pathname);
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
