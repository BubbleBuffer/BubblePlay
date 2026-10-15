import { validCurrentMediaEpoch, validInputStatus, canonicalWorkerVersionId, validFirstFrameMessage, benchStatusRejectionReason, utf8Length } from "./protocol-validation.js";
import { validClipboardMessage, validInputCapabilities } from "./protocol-validation.js";
import { validApprovalMessage } from "./protocol-validation.js";
import { validDesktopState, validSecureAction, validSecureActionResult } from "./protocol-validation.js";
import { approvalMethods } from "./approval-methods.js";
export { benchStatusRejectionReason } from "./protocol-validation.js";
// BubblePlay signaling relay.
//
// One Cloudflare Worker doing two jobs:
//   1. Static hosting of the client page (worker/public/) via Workers Assets.
//   2. A WebSocket relay pairing exactly one host with one client.
//
// Wire protocol (JSON text frames), shared with host/src/signaling.rs:
//   host   -> worker : {type:"hello", token, role:"host"}
//   client -> worker : {type:"hello", role:"client"} after cookie auth
//   worker -> any    : {type:"hello-ok", workerVersionId?} | {type:"error", message}
//   client -> worker : {type:"offer", sdp} | {type:"ice", candidate}
//                       | {type:"diag", seq, atMs, profile}
//                       | {type:"qos", sessionId, seq, atMs, rx, presentation}
//   host   -> worker : {type:"answer", sdp} | {type:"ice", candidate}
//                       | {type:"quality", profile, width, height, fps,
//                          targetBitrateBps, bweBps, reason}
//   worker -> either : {type:"peer-left"} when the other side disconnects
//   either -> worker : {type:"bye"} (graceful exit)
//   either -> worker : {type:"ping"} (keepalive, consumed, never relayed)
//
import { createTurnIceServers } from "./turn.js";
import { ApprovalGate, authorized } from "./gate.js";
import { canonicalDeviceId, deriveDeviceId, randomTicket, hashTicket, constantTimeEqual, verifyRelayProof } from "./device-keys.js";
import { STREAM_LAB_COMPATIBILITY } from "../public/media.js";
import {
  V4_HEADER_BYTES,
  V4_MAX_PACKET_BYTES,
  validateClientMessageV4,
  validateHostTextV4,
} from "../public/media-v4.js";
import { relayAcceptsFilePacket, validFileOffer, FT_MAX_BATCH_FILES, FT_MAX_NAME_BYTES } from "../public/file-transfer.js";

export { ApprovalGate };

// The host's relay credential is separate from browser password authentication.

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    if (url.pathname.startsWith("/__approval/") || url.pathname.startsWith("/__pc/") ||
        ["/__auth/socket", "/__auth/socket-register", "/__auth/socket-unregister", "/__auth/recheck"].includes(url.pathname)) return badRoute();
    if (url.pathname === "/lab-compat" && ["GET", "HEAD"].includes(request.method)) {
      const body = request.method === "HEAD" ? null : JSON.stringify({
        ok: true,
        compatibility: STREAM_LAB_COMPATIBILITY,
      });
      return new Response(body, {
        status: 200,
        headers: {
          "Content-Type": "application/json; charset=utf-8",
          "Cache-Control": "no-store",
        },
      });
    }
    const gateId = env.GATE.idFromName("global-v1");
    const gate = env.GATE.get(gateId);
    const localLabBypass = isLocalLabSocket(request, url, env);
    const socketPath = isHostSocketPath(url.pathname) || isBrowserSocketPath(url.pathname);
    if (isLocalStreamLabRequest(request, url, env) && ["GET", "HEAD"].includes(request.method) && !socketPath) {
      return env.ASSETS.fetch(request);
    }
    // Browsers attach cookies to cross-site WebSocket upgrades, so a socket
    // opened by another site's page is refused here. Native peers (the host,
    // the Android client) send no Origin header and are unaffected.
    if (socketPath && !sameOriginUpgrade(request, url)) return badRoute();
    // Only the credential-free local lab socket may run without real secrets.
    if (!localLabBypass) {
      const weak = weakSecretNames(env);
      if (weak.length) return configurationError(weak);
      try { approvalMethods(env); } catch (error) {
        return new Response(`Relay configuration error: ${error.message}`, {
          status: 500, headers: { "Cache-Control": "no-store", "Content-Type": "text/plain; charset=utf-8" },
        });
      }
    }
    if (isHostSocketPath(url.pathname)) {
      // Signaling always needs the phase-1 endpoint bearer. Media/file may
      // instead authenticate with a signaling ticket in their first frame.
      if ((!isMediaSocketPath(url.pathname) && !isFileSocketPath(url.pathname) || request.headers.has("Authorization")) &&
          !localLabBypass && !(await authorized(request, env.HOST_GATE_TOKEN))) {
        return new Response("Not found", { status: 404, headers: { "Cache-Control": "no-store" } });
      }
      if (!validSocketRoute(url)) return badRoute();
      return handleWebSocket(request, env);
    }
    if (isBrowserSocketPath(url.pathname)) {
      if (!validSocketRoute(url)) return badRoute();
      if (!localLabBypass) {
        const checkHeaders = new Headers({ Cookie: request.headers.get("Cookie") || "" });
        const versionOverrides = request.headers.get("Cloudflare-Workers-Version-Overrides");
        if (versionOverrides)
          checkHeaders.set("Cloudflare-Workers-Version-Overrides", versionOverrides);
        const check = new Request("https://gate.internal/__auth/check", {
          headers: checkHeaders,
        });
        if ((await gate.fetch(check)).status !== 204) {
          return new Response("Not found", { status: 404, headers: { "Cache-Control": "no-store" } });
        }
      }
      if (url.pathname === "/ws" && !url.searchParams.has("pc")) {
        const resolved = await gate.fetch(new Request("https://gate.internal/__pc/resolve"));
        if (resolved.status !== 200) return badRoute();
        const { pc } = await resolved.json();
        if (pc !== null) {
          if (!canonicalDeviceId(pc)) return badRoute();
          url.searchParams.set("pc", canonicalDeviceId(pc));
          // Clone the original request, including its Upgrade header.
          request = new Request(url, request);
        }
      }
      return handleWebSocket(request, env);
    }
    return gate.fetch(request);
  },
};

function socketRoom(url) {
  // Room name bump = fresh Durable Object state. The previous "room" instance
  // accumulated stale sockets/buffers across several force-killed sessions
  // (phone attempt, UAC crash, test browsers), which poisoned the relay: a
  // stale offer from an old session was flushed to a new host, whose DTLS
  // certificate then didn't match ("remote fingerprint no match").
  const media = isMediaSocketPath(url.pathname);
  const file = isFileSocketPath(url.pathname);
  const lab = isLabSocketPath(url.pathname);
  // File sockets reuse the MediaRoom class (same pairing code) under their
  // own room names, so no new Durable Object class or migration is needed.
  const epoch = url.searchParams.get("epoch");
  const run = url.searchParams.get("run");
  const pc = canonicalDeviceId(url.searchParams.get("pc"));
  // One media Durable Object per browser epoch carries the single v4 socket
  // pair (media down, ACK/keyframe up).
  const roomName = file
    ? (lab ? `lab-file-v1:${run}:${epoch}` : `file-v1:${epoch}`)
    : pc && !media ? `pc-v1:${pc}` : lab
      ? (media ? `lab-media-v4:${run}:${epoch}` : `lab-signal-v1:${run}`)
      : (media ? `media-v4:${epoch}` : "room-v3");
  return { roomName, namespace: media || file ? "MEDIA_ROOM" : "ROOM" };
}

function handleWebSocket(request, env) {
  const { roomName, namespace: binding } = socketRoom(new URL(request.url));
  const namespace = env[binding];
  const roomId = namespace.idFromName(roomName);
  const stub = namespace.get(roomId);
  // Pass the ORIGINAL request straight through. Reconstructing headers here
  // strips the hop-by-hop `Upgrade: websocket` header, and the DO then rejects
  // the 101 WebSocket response ("request did not contain Upgrade: websocket").
  return stub.fetch(request);
}

function isHostSocketPath(pathname) {
  return pathname === "/host-ws" || pathname === "/host-media-ws" ||
    pathname === "/lab-host-ws" || pathname === "/lab-host-media-ws" ||
    pathname === "/host-file-ws" || pathname === "/lab-host-file-ws" ||
    pathname === "/host-version-ws";
}
function isBrowserSocketPath(pathname) {
  return pathname === "/ws" || pathname === "/media-ws" ||
    pathname === "/lab-ws" || pathname === "/lab-media-ws" ||
    pathname === "/file-ws" || pathname === "/lab-file-ws";
}
function isMediaSocketPath(pathname) { return pathname.endsWith("media-ws"); }
function isFileSocketPath(pathname) { return pathname.endsWith("file-ws"); }
function isLabSocketPath(pathname) { return pathname.startsWith("/lab-"); }
// Lab only. The URL hostname comes from the client's Host header wherever
// the relay is not behind Cloudflare's hostname routing, so it never decides
// "local" alone: the variable must be set explicitly and every client-address
// header a proxy or runtime attached must name a loopback address. A relay
// reachable without such a header must leave the variable unset.
const CLIENT_ADDRESS_HEADERS = ["CF-Connecting-IP", "True-Client-IP", "X-Real-IP", "X-Forwarded-For", "Forwarded"];
function isLocalStreamLabRequest(request, url, env) {
  return env.STREAM_LAB_LOCAL_BYPASS === "1" &&
    ["127.0.0.1", "localhost", "[::1]"].includes(url.hostname) &&
    CLIENT_ADDRESS_HEADERS.every((name) => loopbackAddressList(name, request.headers.get(name)));
}
function loopbackAddressList(name, value) {
  if (value === null) return true;
  const entries = name === "Forwarded"
    ? value.split(/[;,]/).map((part) => /^\s*for=(.*)$/i.exec(part)?.[1]).filter((entry) => entry !== undefined)
    : value.split(",");
  return entries.length > 0 && entries.every(isLoopbackAddress);
}
function isLoopbackAddress(entry) {
  const address = entry.trim().replace(/^"|"$/g, "").replace(/^\[([^\]]*)\](?::\d+)?$/, "$1")
    .replace(/^(\d+\.\d+\.\d+\.\d+):\d+$/, "$1").toLowerCase();
  return /^(?:::ffff:)?127\.\d{1,3}\.\d{1,3}\.\d{1,3}$/.test(address) || address === "::1";
}
function isLocalLabSocket(request, url, env) {
  return isLocalStreamLabRequest(request, url, env) && isLabSocketPath(url.pathname) && validSocketRoute(url);
}
function sameOriginUpgrade(request, url) {
  const origin = request.headers.get("Origin");
  return origin === null || origin === url.origin;
}
// Guessable credentials are a deployment error, not a weaker mode: the relay
// answers nothing until every secret meets its minimum length.
const MIN_SECRET_LENGTHS = { TOKEN: 32, HOST_GATE_TOKEN: 32, GATE_ADMIN_TOKEN: 32, APP_PASSWORD: 12 };
export function weakSecretNames(env) {
  return Object.entries(MIN_SECRET_LENGTHS)
    .filter(([name, min]) => typeof env?.[name] !== "string" || env[name].length < min)
    .map(([name]) => name);
}
function configurationError(names) {
  const message = "Relay configuration error: " + names
    .map((name) => `${name} must be set to at least ${MIN_SECRET_LENGTHS[name]} characters`).join("; ") + ".";
  console.log(`[config] ${message}`);
  return new Response(message, {
    status: 500,
    headers: { "Cache-Control": "no-store", "Content-Type": "text/plain; charset=utf-8" },
  });
}
function validSocketRoute(url) {
  if (url.searchParams.has("pc") &&
      (url.searchParams.getAll("pc").length !== 1 || !canonicalDeviceId(url.searchParams.get("pc")))) return false;
  if (isLabSocketPath(url.pathname) && !validLabRun(url)) return false;
  if (!isMediaSocketPath(url.pathname) && !isFileSocketPath(url.pathname)) return true;
  return /^[a-f0-9]{32}$/.test(url.searchParams.get("epoch") || "");
}
function validLabRun(url) {
  return /^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i
    .test(url.searchParams.get("run") || "") &&
    url.searchParams.get("compat") === STREAM_LAB_COMPATIBILITY.id;
}
function badRoute() { return new Response("Not found", { status: 404, headers: { "Cache-Control": "no-store" } }); }

export class Room {
  constructor(state, env) {
    this.state = state;
    this.env = env;
    // This cache is opportunistic only. Durable Object hibernation may create a
    // fresh Room at any time, so socket attachments remain authoritative and a
    // cache miss always rebuilds from state.getWebSockets().
    this.peerCache = new Map();
    this.sessionChecks = new Map();
    this.pendingOperation = Promise.resolve();
    this.messageOperation = Promise.resolve();
    if (env.GATE && state.blockConcurrencyWhile && state.getWebSockets().some(socket => {
      const att = socket.deserializeAttachment() || {};
      return socket.readyState === 1 && !att.replaced && (att.role || att.expectedRole)?.endsWith("client");
    })) {
      // Revalidate attachments on wake, including sockets accepted before
      // session binding was introduced, before allowing peer traffic.
      state.blockConcurrencyWhile(async () => {
        await this.checkBrowserSockets();
        await this.scheduleAlarm();
      });
    }
  }

  async fetch(request) {
    // Remember the query token + role on the socket itself: with the
    // hibernation API the DO class is re-instantiated per wake, so instance
    // fields are not authoritative peer state. Attachments and storage are.
    const url = new URL(request.url);
    if (url.origin === "https://room.internal") {
      if (url.pathname.startsWith("/__pc/")) return this.pcFetch(request, url);
      if (url.pathname.startsWith("/__approval/")) return this.approvalFetch(request, url);
      if (url.pathname === "/__auth/recheck" && request.method === "POST") {
        await this.checkBrowserSockets();
        return new Response(null, { status: 204 });
      }
      return badRoute();
    }
    if (!validSocketRoute(url)) return badRoute();
    const media = isMediaSocketPath(url.pathname);
    const file = isFileSocketPath(url.pathname);
    const lab = isLabSocketPath(url.pathname);
    const localLab = isLocalLabSocket(request, url, this.env);
    const expectedRole = url.pathname === "/host-version-ws" ? "version-probe" :
      file ? (isHostSocketPath(url.pathname) ? "file-host" : "file-client") :
      media ? (isHostSocketPath(url.pathname) ? "media-host" : "media-client") :
        (isHostSocketPath(url.pathname) ? "host" : "client");
    let sessionIdentity = null;
    const location = socketRoom(url);
    if (expectedRole.endsWith("client") && !localLab) {
      const check = await this.gateFetch("/__auth/socket", { headers: { Cookie: request.headers.get("Cookie") || "" } });
      if (check.status !== 200) return badRoute();
      sessionIdentity = await check.json();
      if (!validSocketIdentity(sessionIdentity)) return badRoute();
      const registration = await this.gateFetch("/__auth/socket-register", {
        method: "POST", body: JSON.stringify({ ...location, identity: sessionIdentity }),
      });
      if (registration.status !== 204) return badRoute();
      await this.scheduleAlarm();
    }
    const pair = new WebSocketPair();
    const [client, server] = Object.values(pair);
    console.log("[room] socket accepted");
    server.serializeAttachment({ role: null, expectedRole, sessionIdentity, ...location,
      pc: canonicalDeviceId(url.searchParams.get("pc")), origin: url.origin,
      bearerOk: isHostSocketPath(url.pathname) && (localLab || await authorized(request, this.env.HOST_GATE_TOKEN)),
      epoch: url.searchParams.get("epoch") || null,
      run: lab ? url.searchParams.get("run") : null,
      localLab });

    // acceptWebSocket() both accepts the server side and registers it for
    // hibernation-driven webSocketMessage/webSocketClose callbacks.
    this.state.acceptWebSocket(server);
    return new Response(null, { status: 101, webSocket: client });
  }

  webSocketMessage(ws, message) {
    // Hello/proof crypto and DO calls yield. Preserve wire order and single
    // peer ownership while those awaits run, including concurrent hellos.
    const operation = this.messageOperation.then(() => this.onWebSocketMessage(ws, message)).catch(error => {
      const att = ws.deserializeAttachment() || {};
      if (!att.role && (att.pc || att.expectedRole?.startsWith("media-") || att.expectedRole?.startsWith("file-"))) {
        this.closePeer(ws, 1008, "");
        return;
      }
      throw error;
    });
    this.messageOperation = operation.catch(() => {});
    return operation;
  }

  async onWebSocketMessage(ws, message) {
    let att = ws.deserializeAttachment() || {};
    if (att.replaced || ws.readyState !== 1) return;
    if (!(await this.checkBrowserSocket(ws, att))) return;
    if (att.role === "media-host" || att.role === "media-client") {
      this.relayMediaV4(ws, att, message);
      return;
    }
    if (att.role === "file-host" || att.role === "file-client") {
      this.relayFile(ws, att, message);
      return;
    }
    // WSS media leaves input and cursor on the pre-existing authenticated
    // signaling relay. Those packets are small binary control messages, never
    // video, and are bounded independently from media packets.
    if ((att.role === "client" || att.role === "host") && binaryLength(message) > 0) {
      if (binaryLength(message) <= 16_384) {
        const target = att.role === "client" ? this.findPeer("host") : this.findPeer("client");
        if (target) target.send(message);
      }
      return;
    }
    let msg;
    if (typeof message !== "string") return;
    if (utf8Length(message) > SIGNAL_MAX_BYTES) {
      this.closePeer(ws, 1009, "signaling message too large");
      return;
    }
    try {
      msg = JSON.parse(message);
    } catch {
      return; // not JSON: ignore
    }
    if (!msg || typeof msg !== "object" || Array.isArray(msg)) return;

    if (msg.type === "approval-decision") {
      if (att.role !== "host" || !validApprovalMessage(msg)) return;
      const expiresAt = att.approvalRequests?.[msg.id];
      if (!expiresAt || expiresAt <= Date.now()) return;
      const approvalRequests = { ...att.approvalRequests };
      delete approvalRequests[msg.id];
      ws.serializeAttachment({ ...att, approvalRequests });
      await this.gateFetch("/__approval/decision", { method: "POST", body: message });
      return;
    }
    if (msg.type === "approval-request" || msg.type === "approval-cancel") return;

    if (msg.type !== "diag") {
      console.log(`[room] msg type=${msg.type} role=${att.role || "unassigned"}`);
    }
    if (msg.type === "ping") {
      // Application-level acknowledgement makes half-open/stale Durable Object
      // sockets observable to native hosts. TCP writes alone can succeed for a
      // long time after the relay-side association has disappeared.
      ws.send(JSON.stringify({ type: "pong" }));
      return; // keepalive acknowledgement is never relayed
    }

    let registeredByKey = false;
    if (!att.role && att.registration) {
      const registration = att.registration;
      // Consume the nonce before crypto/DO awaits: a second proof cannot race
      // the first proof, and a wake cannot resurrect a consumed challenge.
      att = { ...att, registration: null };
      ws.serializeAttachment(att);
      if (msg.type !== "register-proof" || registration.expiresAt <= Date.now() ||
          (registration.tokenSupplied && (typeof this.env.TOKEN !== "string" ||
            !constantTimeEqual(registration.tokenHash, await hashTicket(this.env.TOKEN)))) ||
          !(await verifyRelayProof({ ...registration, origin: att.origin, signature: msg.signature }))) {
        this.closePeer(ws, 1008, "");
        return;
      }
      const registered = await this.gateFetch("/__pc/register", { method: "POST", body: JSON.stringify({
        id: att.pc, public: registration.public,
        ...(registration.tokenSupplied ? { token: this.env.TOKEN } : {}),
      }) });
      if (registered.status !== 204 || ws.readyState !== 1 || ws.deserializeAttachment()?.replaced) {
        this.closePeer(ws, 1008, "");
        return;
      }
      registeredByKey = true;
      msg = { type: "hello", role: "host" };
    }

    // First message must be hello; assigns the socket a role.
    if (!att.role) {
      if (msg.type !== "hello") return;
      const configuredToken = this.env.TOKEN;
      const role = msg.role;
      if (!["host", "client", "media-host", "media-client", "file-host", "file-client", "version-probe"].includes(role) ||
          role !== att.expectedRole) {
        if (att.expectedRole === "host" && att.pc) this.closePeer(ws, 1008, "");
        else this.reject(ws, "bad role");
        return;
      }
      if (role === "host" && !registeredByKey && (att.pc || Object.hasOwn(msg, "id"))) {
        const id = canonicalDeviceId(msg.id);
        if (!att.pc || id !== att.pc || await deriveDeviceId(msg.public) !== id) {
          this.closePeer(ws, 1008, "");
          return;
        }
        const lookup = await this.gateFetch("/__pc/lookup", { method: "POST", body: JSON.stringify({
          id, public: msg.public, ...(Object.hasOwn(msg, "token") ? { token: msg.token } : {}),
        }) });
        if (lookup.status !== 204 || ws.readyState !== 1 || ws.deserializeAttachment()?.replaced) {
          this.closePeer(ws, 1008, "");
          return;
        }
        const nonce = randomTicket();
        const tokenHash = Object.hasOwn(msg, "token") ? await hashTicket(msg.token) : null;
        if (ws.readyState !== 1 || ws.deserializeAttachment()?.replaced) return;
        ws.serializeAttachment({ ...att, registration: {
          nonce, id, public: msg.public, tokenHash, tokenSupplied: Object.hasOwn(msg, "token"), expiresAt: Date.now() + 60_000,
        } });
        ws.send(JSON.stringify({ type: "register-challenge", nonce }));
        return;
      }
      const auxiliary = role.startsWith("media-") || role.startsWith("file-");
      if (auxiliary && !(await this.authenticateAuxiliary(att, msg))) {
        this.closePeer(ws, 1008, "");
        return;
      }
      // Auxiliary peers authenticate against the signaling Room above; they
      // never need to duplicate the registration secret in their hello.
      const tokenOk = registeredByKey || role === "client" || role === "media-client" || role === "media-host" ||
        role === "file-client" || role === "file-host" ||
        (role === "host" && att.localLab === true && this.env.STREAM_LAB_LOCAL_BYPASS === "1") ||
        (typeof configuredToken === "string" && configuredToken.length > 0 && msg.token === configuredToken);
      if (!tokenOk) {
        this.reject(ws, "bad token");
        return;
      }

      const workerVersionId = canonicalWorkerVersionId(this.env.WORKER_VERSION?.id)
        ? this.env.WORKER_VERSION.id : null;
      if (role === "version-probe") {
        ws.send(JSON.stringify({ type: "hello-ok", ...(workerVersionId ? { workerVersionId } : {}) }));
        ws.close(1000, "version-probe-complete");
        return;
      }

      // Exactly one host + one client: kick any stale socket of the same role.
      const existing = this.findPeer(role);
      const existingAttachment = existing?.deserializeAttachment?.() || {};
      console.log(`[room] hello role=${role} run=${att.run || "-"} existing=${existing ? "yes" : "none"} existingTarget=${existingAttachment.registeredAtMs ?? "-"} existingReplaced=${!!existingAttachment.replaced}`);
      if (existing && existing !== ws) {
        // A replacement signaling host has no control or media state from the
        // old host. Force the live browser to establish a fresh signaling
        // epoch so capabilities, topology, and input bind together.
        if (role === "host") {
          const client = this.findPeer("client");
          await this.retireHostSockets();
          try { client?.send(JSON.stringify({ type: "peer-left" })); } catch { /* client already gone */ }
        }
        this.reject(existing, `${role} replaced`);
      }
      // A fresh client means any buffered offer/ICE came from a previous,
      // now-dead session — delivering them to a future host would poison it
      // (the host accepts a stale offer, its DTLS times out, session dies).
      if (role === "client") {
        if (existingAttachment.mediaEpoch) await this.retireEpoch(existingAttachment);
        await this.withPending(() => this.state.storage.delete("pendingToHost"));
      }
      const mediaEpoch = role === "client" ? randomEpoch() : null;
      const registeredAtMs = Date.now();
      const ticket = role === "host" || role === "client" ? randomTicket() : null;
      const ticketBinding = ticket ? { ticketHash: await hashTicket(ticket), ticketExpiresAt: Date.now() + TICKET_TTL_MS } : {};
      if (ws.readyState !== 1 || ws.deserializeAttachment()?.replaced) return;
      ws.serializeAttachment(role === "client"
        ? { ...att, role, mediaEpoch, registeredAtMs, ...ticketBinding }
        : { ...att, role, registeredAtMs, ...ticketBinding });
      this.rememberPeer(role, ws);
      // Only the digest lives in the hibernation attachment; the ticket is
      // delivered once, on the authenticated signaling connection.
      const hello = role === "client"
        ? { type: "hello-ok", iceServers: await createTurnIceServers(this.env, { turn: msg.turn === true }), mediaEpoch,
            ...(workerVersionId ? { workerVersionId } : {}) }
        : { type: "hello-ok",
            ...(role === "media-client" ? { registeredAtMs } : {}),
            ...(workerVersionId ? { workerVersionId } : {}) };
      if (ticket) Object.assign(hello, { ticket, ...(att.pc ? { pc: att.pc } : {}) });
      ws.send(JSON.stringify(hello));
      if (role === "host") await this.reportPresence(att.pc);
      if (role === "media-host" || role === "media-client") {
        // v4 peers learn about each other from the relay: the host needs a
        // fresh keyframe for every new receiver and must not encode for an
        // empty room.
        const other = this.findPeer(role === "media-host" ? "media-client" : "media-host");
        ws.send(JSON.stringify({ type: "media-peer", present: !!other }));
        try { other?.send(JSON.stringify({ type: "media-peer", present: true })); } catch { /* closing */ }
      }
      if (role === "file-host" || role === "file-client") {
        // A replacement peer starts with no transfer state: the other side
        // learns about it and resets.
        const other = this.findPeer(role === "file-host" ? "file-client" : "file-host");
        ws.send(JSON.stringify({ type: "file-peer", present: !!other }));
        try { other?.send(JSON.stringify({ type: "file-peer", present: true })); } catch { /* closing */ }
      }
      console.log(`[room] ${role} registered run=${att.run || "-"} target=${registeredAtMs}, hello-ok sent`);
      if (role === "client") {
        const hostCapabilities = this.findPeer("host")?.deserializeAttachment()?.hostCapabilities;
        if (typeof hostCapabilities === "string") ws.send(hostCapabilities);
        const inputCapabilities = this.findPeer("host")?.deserializeAttachment()?.inputCapabilities;
        if (typeof inputCapabilities === "string") ws.send(inputCapabilities);
      }

      // Flush anything the client sent before the host arrived.
      if (role === "host") {
        await this.withPending(async () => {
          const pending = (await this.state.storage.get("pendingToHost")) || [];
          const pendingBytes = pending.reduce((total, raw) => total + (typeof raw === "string" ? utf8Length(raw) : PENDING_MAX_BYTES + 1), 0);
          if (pending.length > 64 || pendingBytes > PENDING_MAX_BYTES) {
            await this.state.storage.delete("pendingToHost");
            const client = this.findPeer("client");
            if (client) this.closePeer(client, 1013, "pending queue full");
            return;
          }
          if (pending.length) {
            console.log(`[room] flushing ${pending.length} buffered msg(s) to host`);
            // The buffered messages came FROM the client and go TO the host —
            // which is this socket, `ws`.
            for (const m of pending) {
              if (typeof m !== "string" || utf8Length(m) > SIGNAL_MAX_BYTES) continue;
              if (!this.sendPeer(ws, m)) break;
            }
            await this.state.storage.delete("pendingToHost");
          }
        });
      }
      return;
    }

    // Relay to the peer.
    if (msg.type === "client-log" &&
        (att.role !== "client" || typeof message !== "string" || utf8Length(message) > 2_048 ||
         typeof msg.seq !== "number" || typeof msg.atMs !== "number" || typeof msg.level !== "string" ||
         typeof msg.event !== "string" || !msg.detail || typeof msg.detail !== "object")) {
      return;
    }
    if (msg.type === "diag" &&
        (att.role !== "client" || typeof message !== "string" || message.length > 16_384)) {
      return;
    }
    // QoS is page-scoped receiver telemetry sent only from the browser client;
    // quality is the host's bounded response. Drop invalid control frames while
    // keeping both otherwise healthy peers connected.
    if (msg.type === "qos" &&
        (att.role !== "client" || typeof message !== "string" || utf8Length(message) > 2_048)) {
      return;
    }
    if (msg.type === "quality" &&
        (att.role !== "host" || typeof message !== "string" || utf8Length(message) > 1_024)) {
      return;
    }
    if (msg.type === "bench-status") {
      const reason = benchStatusRejectionReason(msg, att.role, message);
      if (reason) {
        if (att.run && att.role === "host") {
          const target = this.findPeer("client");
          target?.send(JSON.stringify({
            type: "bench-status-rejected",
            protocol: 1,
            reason,
          }));
          console.log(`[room] bench-status rejected run=${att.run} reason=${reason}`);
        }
        return;
      }
    }
    const approvalOnly = msg.type === "host-capabilities" &&
      Object.keys(msg).length === 2 && validApprovalMessage(msg);
    if (msg.type === "host-capabilities" &&
        (att.role !== "host" || typeof message !== "string" || utf8Length(message) > 512 ||
         (!approvalOnly && (typeof msg.serviceMode !== "boolean" || typeof msg.coldBoot !== "boolean" ||
         typeof msg.secureDesktop !== "boolean" || typeof msg.secureText !== "boolean")) ||
         (msg.browserApproval !== undefined && typeof msg.browserApproval !== "boolean"))) {
      return;
    }
    if (msg.type === "host-capabilities" && msg.fileTransfer !== undefined &&
        !(Number.isInteger(msg.fileTransfer) && msg.fileTransfer >= 0 && msg.fileTransfer <= 255)) {
      return;
    }
    if (msg.type === "host-capabilities" && msg.fileDrop !== undefined &&
        !(msg.fileDrop === 1 && msg.fileTransfer === 1)) {
      return;
    }
    if (msg.type === "host-capabilities" && msg.fileDropPreview !== undefined &&
        !(msg.fileDropPreview === 1 && msg.fileDrop === 1 && msg.fileTransfer === 1 && msg.serviceMode === false)) {
      return;
    }
    if (msg.type === "host-capabilities" && msg.followWindow !== undefined &&
        !(Number.isInteger(msg.followWindow) && msg.followWindow >= 0 && msg.followWindow <= 255)) {
      return;
    }
    if (msg.type === "file-offer" &&
        (att.role !== "host" || typeof message !== "string" || utf8Length(message) > FILE_OFFER_MAX_RAW_BYTES ||
         !validFileOffer(msg))) {
      return;
    }
    if (msg.type === "host-capabilities") {
      ws.serializeAttachment({ ...att, hostCapabilities: message, browserApproval: msg.browserApproval === true });
    }
    if (msg.type === "desktop-state" && !validDesktopState(msg, att.role, message)) return;
    if (msg.type === "secure-action" && !validSecureAction(msg, att.role, message)) return;
    if (msg.type === "secure-action-result" && !validSecureActionResult(msg, att.role, message)) return;
    if (msg.type === "input-status" && !validInputStatus(msg, att.role, message)) return;
    if ((msg.type === "clipboard" || msg.type === "clipboard-image") && !validClipboardMessage(msg, att.role, message)) return;
    if (msg.type === "input-capabilities") {
      if (!validInputCapabilities(msg, att.role, message)) return;
      ws.serializeAttachment({ ...ws.deserializeAttachment(), inputCapabilities: message });
    }
    if (msg.type === "first-frame" && !validFirstFrameMessage(msg, att.role, message)) {
      return;
    }
    if (msg.type === "media-control" &&
        (att.role !== "client" || typeof message !== "string" || utf8Length(message) > 2_048)) {
      return;
    }
    if (msg.type === "file-drag-preview" && !validFileDragPreview(msg, att, message)) return;
    if (msg.type === "media-control" && msg.protocol === 4 &&
        !validCurrentMediaEpoch(msg.epoch, att)) return;
    if (msg.type === "media-control" && msg.action === "monitor" &&
        (att.role !== "client" || typeof msg.monitor !== "string" || msg.monitor.length === 0 || msg.monitor.length > 128)) {
      return;
    }
    if (msg.type === "media-control" && msg.action === "monitors" &&
        (att.role !== "client" || msg.protocol !== 4 || typeof msg.epoch !== "string" ||
         !/^[a-f0-9]{32}$/.test(msg.epoch) || msg.epoch !== att.mediaEpoch)) {
      return;
    }
    // Follow active window: a session-scoped boolean the host applies; v4 and
    // the client socket's current epoch only, like the monitor refresh.
    if (msg.type === "media-control" && msg.action === "follow" &&
        (att.role !== "client" || msg.protocol !== 4 || typeof msg.follow !== "boolean" ||
         !validCurrentMediaEpoch(msg.epoch, att))) {
      return;
    }
    const target = att.role === "host" ? this.findPeer("client") : this.findPeer("host");
    if (msg.type !== "diag" && msg.type !== "client-log") {
      console.log(`[room] relay ${msg.type} -> ${target ? "peer" : "NO PEER"}`);
    }
    if (msg.type === "bye") {
      if (target) {
        target.send(JSON.stringify({ type: "peer-left" }));
      }
      ws.close(1000, "bye");
      return;
    }
    if (!target) {
      // Buffer offer/ICE and the one idempotent WSS start command until the
      // host connects. Route changes commonly bring the browser back first.
      // ACKs/keyframe requests are epoch-specific and must never be replayed.
      if (att.role === "client" && (msg.type === "offer" || msg.type === "ice" ||
          (msg.type === "media-control" && (msg.action === "start" || msg.action === "monitors" || msg.action === "follow")) ||
          (msg.type === "first-frame" && msg.phase === "request"))) {
        await this.withPending(async () => {
          if (ws.deserializeAttachment()?.replaced) return;
          const liveHost = this.findPeer("host");
          if (liveHost) { this.sendPeer(liveHost, message); return; }
          const pending = (await this.state.storage.get("pendingToHost")) || [];
          if (msg.type === "media-control" || msg.type === "first-frame") {
            for (let i = pending.length - 1; i >= 0; i -= 1) {
              try {
                const old = JSON.parse(pending[i]);
                if ((msg.type === "media-control" && old.type === "media-control" && old.action === msg.action &&
                     (msg.action === "start" || msg.action === "monitors" || msg.action === "follow")) ||
                    (msg.type === "first-frame" && old.type === "first-frame" && old.phase === "request")) pending.splice(i, 1);
              } catch { /* retain validated offer/ICE wire text */ }
            }
          }
          const bytes = pending.reduce((total, raw) => total + (typeof raw === "string" ? utf8Length(raw) : PENDING_MAX_BYTES), 0);
          if (pending.length >= 64 || bytes + utf8Length(message) > PENDING_MAX_BYTES) {
            await this.state.storage.delete("pendingToHost");
            this.closePeer(ws, 1013, "pending queue full");
            return;
          }
          pending.push(message);
          await this.state.storage.put("pendingToHost", pending);
        });
      }
      return;
    }
    this.sendPeer(target, message);
  }

  async webSocketClose(ws, _code, _reason, _wasClean) {
    const att = ws.deserializeAttachment() || {};
    const role = att.role;
    // A closing socket is never authoritative, even when the runtime still
    // returns it from getWebSockets() during this callback.
    ws.serializeAttachment({ ...att, replaced: true });
    this.forgetPeer(ws);
    if (att.sessionIdentity) {
      this.sessionChecks.delete(`${att.sessionIdentity.gateHash}:${att.sessionIdentity.authHash}`);
      const stillAttached = this.state.getWebSockets().some(socket => socket !== ws && socket.readyState === 1 &&
        !socket.deserializeAttachment()?.replaced &&
        socket.deserializeAttachment()?.sessionIdentity?.authHash === att.sessionIdentity.authHash);
      if (!stillAttached) {
        await this.gateFetch("/__auth/socket-unregister", { method: "POST",
          body: JSON.stringify({ roomName: att.roomName, namespace: att.namespace, identity: att.sessionIdentity }) });
      }
    }
    console.log(`[room] socket closed role=${role || "unassigned"} run=${att.run || "-"} target=${att.registeredAtMs ?? "-"} replaced=${!!att.replaced}`);
    if (!role) return;
    if (att.replaced && !att.closedByRelay) return; // replaced sockets get no farewell
    // When the client dies, its buffered offer/ICE is worthless — clear it so
    // a later host never receives a stale offer from a dead peer.
    if (role === "client") {
      await this.retireEpoch(att);
      await this.withPending(() => this.state.storage.delete("pendingToHost"));
    }
    if (role === "host") {
      await this.retireHostSockets();
      await this.reportPresence(att.pc);
    }
    // Media socket churn is isolated from the signaling session. Losing the
    // media socket must not tell the host/browser that the application peer left.
    if (role === "media-host" || role === "media-client") {
      const other = this.findPeer(role === "media-host" ? "media-client" : "media-host");
      try { other?.send(JSON.stringify({ type: "media-peer", present: false })); } catch { /* closing */ }
      return;
    }
    if (role === "file-host" || role === "file-client") {
      // A transfer never outlives either socket: close the other side so it
      // cancels whatever was in flight (no resume in v1).
      const other = this.findPeer(role === "file-host" ? "file-client" : "file-host");
      if (other) {
        try { other.serializeAttachment({ ...(other.deserializeAttachment() || {}), replaced: true }); } catch { /* gone */ }
        this.forgetPeer(other);
        try { other.close(1001, "file-peer-left"); } catch { /* already closed */ }
      }
      return;
    }
    const peer = role === "host" ? this.findPeer("client") : this.findPeer("host");
    if (peer) {
      try {
        peer.send(JSON.stringify({ type: "peer-left" }));
      } catch {
        // peer already gone
      }
    }
  }

  /**
   * Protocol v4: one socket per side. Binary frames flow host -> client;
   * validated JSON ACK / keyframe messages flow client -> host; heartbeats
   * are answered here and never relayed. Nothing is buffered: a message
   * without a live peer is dropped.
   */
  relayMediaV4(ws, att, message) {
    const length = binaryLength(message);
    if (typeof message !== "string") {
      if (att.role !== "media-host" || length <= V4_HEADER_BYTES || length > V4_MAX_PACKET_BYTES) return;
      const target = this.findPeer("media-client");
      if (target) {
        try { target.send(message); } catch { /* receiver closing */ }
      }
      return;
    }
    const parsed = att.role === "media-client" ? validateClientMessageV4(message) : validateHostTextV4(message);
    if (!parsed) return;
    if (parsed.type === "ping") {
      ws.send(JSON.stringify({ type: "pong" }));
      return;
    }
    if (att.role !== "media-client") return;
    const target = this.findPeer("media-host");
    if (target) {
      try { target.send(message); } catch { /* host closing */ }
    }
  }

  /**
   * File transfer v1: binary packets in both directions, each validated on
   * its own (header, type, payload limits, control JSON); REQUEST only from
   * the client. A malformed packet closes the sender, and so its peer.
   * Nothing is buffered: a packet without a live peer is dropped, and the
   * sender's 1 MiB window bounds what is in flight.
   */
  relayFile(ws, att, message) {
    if (typeof message !== "string") {
      if (!relayAcceptsFilePacket(message, att.role)) {
        console.log(`[room] file packet rejected role=${att.role}`);
        const other = this.findPeer(att.role === "file-host" ? "file-client" : "file-host");
        this.reject(ws, "bad file packet");
        if (other) {
          try { other.serializeAttachment({ ...(other.deserializeAttachment() || {}), replaced: true }); } catch { /* gone */ }
          this.forgetPeer(other);
          try { other.close(1001, "file-peer-left"); } catch { /* already closed */ }
        }
        return;
      }
      const target = this.findPeer(att.role === "file-host" ? "file-client" : "file-host");
      if (target) {
        try { target.send(message); } catch { /* peer closing */ }
      }
      return;
    }
    if (message.length > 256) return;
    let msg;
    try { msg = JSON.parse(message); } catch { return; }
    if (msg?.type === "ping") ws.send(JSON.stringify({ type: "pong" }));
  }

  gateFetch(path, init = {}) {
    const gate = this.env.GATE.get(this.env.GATE.idFromName("global-v1"));
    return gate.fetch(new Request(`https://gate.internal${path}`, init));
  }

  signalRoom(pc, run) {
    return this.env.ROOM.get(this.env.ROOM.idFromName(pc ? `pc-v1:${pc}` : run ? `lab-signal-v1:${run}` : "room-v3"));
  }

  async authenticateAuxiliary(att, msg) {
    const host = att.expectedRole.endsWith("host");
    const carriesTicket = Object.hasOwn(msg, "ticket");
    // Direct protocol tests have no namespace. Every deployed media/file
    // socket has one, and performs one validation call to its signaling Room.
    if (!this.env.ROOM) return !carriesTicket && att.bearerOk !== false;
    try {
      const response = await this.signalRoom(att.pc, att.run).fetch(new Request("https://room.internal/__pc/validate", {
        method: "POST", body: JSON.stringify({ pc: att.pc || null, epoch: att.epoch,
          ...(att.run ? { run: att.run } : {}),
          role: host ? "host" : "viewer", requireTicket: host && !att.bearerOk,
          ...(carriesTicket ? { ticket: msg.ticket } : {}) }),
      }));
      return response.status === 204;
    } catch { return false; }
  }

  async pcFetch(request, url) {
    if (url.pathname === "/__pc/online" && request.method === "GET") {
      return Response.json({ online: !!this.findPeer("host") });
    }
    if (request.method !== "POST") return badRoute();
    if (url.pathname === "/__pc/delete") {
      const viewer = this.findPeer("client");
      for (const socket of this.state.getWebSockets()) {
        const att = socket.deserializeAttachment() || {};
        if ((att.role || att.expectedRole) === "host") this.closePeer(socket, 1008, "");
      }
      await this.retireHostSockets();
      if (viewer) {
        this.sendPeer(viewer, JSON.stringify({ type: "peer-left" }));
      }
      return new Response(null, { status: 204 });
    }
    let body;
    try { body = await request.json(); } catch { return badRoute(); }
    if (!body || typeof body !== "object" || Array.isArray(body)) return badRoute();
    if (url.pathname === "/__pc/retire") {
      for (const socket of this.state.getWebSockets()) {
        const att = socket.deserializeAttachment() || {};
        if ((att.pc || null) !== body.pc || att.epoch !== body.epoch) continue;
        if (body.hostOnly && !(att.role || att.expectedRole)?.endsWith("host")) continue;
        this.closePeer(socket, 1001, "");
      }
      return new Response(null, { status: 204 });
    }
    if (url.pathname !== "/__pc/validate" || !["host", "viewer"].includes(body.role) ||
        !(body.pc === null || canonicalDeviceId(body.pc) === body.pc)) return badRoute();
    if (!/^[a-f0-9]{32}$/.test(body.epoch || "")) return badRoute();
    const viewer = this.findPeer("client");
    const carriesTicket = Object.hasOwn(body, "ticket");
    if (!carriesTicket) {
      if (body.requireTicket) return badRoute();
      // Preserve phase-1 bearer/cookie admission for peers without tickets.
      if (body.role === "host") await this.rememberHostEpoch(body);
      return new Response(null, { status: 204 });
    }
    const viewerAtt = viewer?.deserializeAttachment() || {};
    if (body.role === "viewer" && (!viewer || viewerAtt.mediaEpoch !== body.epoch || (viewerAtt.pc || null) !== body.pc)) return badRoute();
    if (typeof body.ticket !== "string" || !/^[A-Za-z0-9_-]{43}$/.test(body.ticket)) return badRoute();
    const digest = await hashTicket(body.ticket);
    const issuingSocket = body.role === "host" ? this.findPeer("host") : viewer;
    const issuing = issuingSocket?.deserializeAttachment() || {};
    if (!issuingSocket || (issuing.pc || null) !== body.pc || !Number.isFinite(issuing.ticketExpiresAt) || issuing.ticketExpiresAt <= Date.now() ||
        typeof issuing.ticketHash !== "string" || !constantTimeEqual(digest, issuing.ticketHash)) return badRoute();
    // Recheck after the hashing await: a socket may have closed/replaced while
    // crypto was running, and its ticket must stop working immediately.
    if (body.role === "host") await this.rememberHostEpoch(body);
    if (this.findPeer(body.role === "host" ? "host" : "client") !== issuingSocket ||
        body.role === "viewer" && this.findPeer("client") !== viewer) return badRoute();
    return new Response(null, { status: 204 });
  }

  async retireEpoch(att, hostOnly = false) {
    if (!att?.mediaEpoch || !this.env.MEDIA_ROOM) return;
    const operations = ["media", "file"].map(kind => {
      const name = att.run ? `lab-${kind}-${kind === "media" ? "v4" : "v1"}:${att.run}:${att.mediaEpoch}`
        : `${kind}-${kind === "media" ? "v4" : "v1"}:${att.mediaEpoch}`;
      return this.env.MEDIA_ROOM.get(this.env.MEDIA_ROOM.idFromName(name)).fetch(new Request("https://room.internal/__pc/retire", {
        method: "POST", body: JSON.stringify({ pc: att.pc || null, epoch: att.mediaEpoch, hostOnly }),
      }));
    });
    const responses = await Promise.all(operations);
    if (responses.some(response => response.status !== 204)) throw new Error("auxiliary cleanup failed");
    if (!hostOnly) await this.withPending(async () => {
      const epochs = await this.state.storage.get("hostAuxiliaryEpochs") || [];
      await this.state.storage.put("hostAuxiliaryEpochs", epochs.filter(epoch => epoch.mediaEpoch !== att.mediaEpoch || epoch.run !== (att.run || null)));
    });
  }

  rememberHostEpoch(body) {
    return this.withPending(async () => {
      const epochs = await this.state.storage.get("hostAuxiliaryEpochs") || [];
      if (epochs.some(epoch => epoch.mediaEpoch === body.epoch && epoch.run === (body.run || null))) return;
      epochs.push({ pc: body.pc, mediaEpoch: body.epoch, run: body.run || null });
      await this.state.storage.put("hostAuxiliaryEpochs", epochs);
    });
  }

  async retireHostSockets() {
    await this.withPending(async () => {
      const epochs = await this.state.storage.get("hostAuxiliaryEpochs") || [];
      await Promise.all(epochs.map(epoch => this.retireEpoch(epoch, true)));
      await this.state.storage.delete("hostAuxiliaryEpochs");
    });
  }

  async reportPresence(pc) {
    if (!this.env.GATE) return;
    await this.gateFetch("/__pc/presence", { method: "POST", body: JSON.stringify({ pc: pc || null, id: pc || null,
      online: !!this.findPeer("host") }) });
  }

  async approvalFetch(request, url) {
    const host = this.findPeer("host");
    const att = host?.deserializeAttachment() || {};
    if (url.pathname === "/__approval/status" && request.method === "GET") {
      return Response.json({ hostOnline: !!host && att.browserApproval === true });
    }
    if (request.method !== "POST") return badRoute();
    let msg;
    const raw = await request.text();
    if (utf8Length(raw) > 1_024) return badRoute();
    try { msg = JSON.parse(raw); } catch { return badRoute(); }
    if (!validApprovalMessage(msg)) return badRoute();
    // Re-read ownership after the request body yields; a replacement host
    // must never inherit another connection's pending dialogs.
    if (!host || this.findPeer("host") !== host) return new Response(null, { status: 503 });
    const current = host.deserializeAttachment() || {};
    const approvalRequests = { ...current.approvalRequests };
    if (url.pathname === "/__approval/request" && msg.type === "approval-request") {
      if (current.browserApproval !== true || msg.expiresAt <= Date.now() ||
          Object.keys(approvalRequests).length >= 10 || approvalRequests[msg.id]) return new Response(null, { status: 503 });
      approvalRequests[msg.id] = msg.expiresAt;
      host.serializeAttachment({ ...current, approvalRequests });
      await this.scheduleAlarm();
      return new Response(null, { status: this.sendPeer(host, raw) ? 204 : 503 });
    }
    if (url.pathname === "/__approval/cancel" && msg.type === "approval-cancel") {
      if (approvalRequests[msg.id]) {
        delete approvalRequests[msg.id];
        host.serializeAttachment({ ...current, approvalRequests });
        this.sendPeer(host, raw);
      }
      return new Response(null, { status: 204 });
    }
    return badRoute();
  }

  async checkBrowserSocket(ws, att, force = false) {
    if ((att.localLab && this.env.STREAM_LAB_LOCAL_BYPASS === "1") ||
        !(att.role || att.expectedRole)?.endsWith("client")) return true;
    // Real Rooms always have GATE; direct protocol unit tests provide only
    // peers. Browser admission itself requires a valid gate identity.
    if (!this.env.GATE && !att.sessionIdentity) return true;
    const identity = att.sessionIdentity;
    if (!validSocketIdentity(identity)) {
      this.closePeer(ws, 1008, "browser session revoked");
      return false;
    }
    const key = `${identity.gateHash}:${identity.authHash}`;
    let cached = this.sessionChecks.get(key);
    if (force || !cached || cached.until <= Date.now()) {
      cached = { until: Date.now() + SESSION_CACHE_MS,
        valid: this.gateFetch("/__auth/socket", { method: "POST", body: JSON.stringify(identity) })
          .then(response => response.status === 204, () => false) };
      this.sessionChecks.set(key, cached);
    }
    const valid = await cached.valid;
    if (!valid) {
      this.closePeer(ws, 1008, "browser session revoked");
      if (att.role === "client" || att.expectedRole === "client") {
        await this.withPending(() => this.state.storage.delete("pendingToHost"));
      }
    }
    return valid && !ws.deserializeAttachment()?.replaced;
  }

  async checkBrowserSockets() {
    this.sessionChecks.clear();
    for (const socket of this.state.getWebSockets()) {
      const att = socket.deserializeAttachment() || {};
      if (socket.readyState === 1 && !att.replaced) await this.checkBrowserSocket(socket, att);
    }
  }

  async scheduleAlarm() {
    if (!this.state.storage.setAlarm) return;
    const next = Date.now() + SESSION_CHECK_MS;
    const current = await this.state.storage.getAlarm();
    if (current === null || current > next) await this.state.storage.setAlarm(next);
  }

  async alarm() {
    await this.checkBrowserSockets();
    let needed = false;
    for (const socket of this.state.getWebSockets()) {
      const att = socket.deserializeAttachment() || {};
      if (socket.readyState !== 1 || att.replaced) continue;
      if (att.sessionIdentity) needed = true;
      const approvalRequests = { ...att.approvalRequests };
      for (const [id, expiresAt] of Object.entries(approvalRequests)) {
        if (expiresAt <= Date.now()) {
          delete approvalRequests[id];
          this.sendPeer(socket, JSON.stringify({ type: "approval-cancel", id }));
        } else needed = true;
      }
      if (att.approvalRequests) socket.serializeAttachment({ ...socket.deserializeAttachment(), approvalRequests });
    }
    if (needed) await this.state.storage.setAlarm(Date.now() + SESSION_CHECK_MS);
  }

  withPending(operation) {
    const next = this.pendingOperation.then(operation);
    this.pendingOperation = next.catch(() => {});
    return next;
  }

  sendPeer(ws, message) {
    if (ws.readyState !== 1 || ws.deserializeAttachment()?.replaced) return false;
    try { ws.send(message); return true; } catch {
      this.closePeer(ws, 1013, "peer send failed");
      return false;
    }
  }

  closePeer(ws, code, reason) {
    const att = ws.deserializeAttachment() || {};
    if (att.sessionIdentity) this.sessionChecks.delete(`${att.sessionIdentity.gateHash}:${att.sessionIdentity.authHash}`);
    try { ws.serializeAttachment({ ...att, replaced: true, closedByRelay: true }); } catch { /* gone */ }
    this.forgetPeer(ws);
    try { ws.close(code, reason); } catch { /* already closed */ }
  }

  reject(ws, message) {
    console.log(`[room] REJECT: ${message}`);
    // Make the durable attachment non-authoritative before touching the wire.
    // A half-closed WebSocket can throw from send(), and hibernation may later
    // rebuild peer ownership only from attachments returned by getWebSockets().
    try {
      const att = ws.deserializeAttachment() || {};
      if (att.role) ws.serializeAttachment({ ...att, replaced: true });
    } catch { /* attachment already unavailable */ }
    this.forgetPeer(ws);
    try { ws.send(JSON.stringify({ type: "error", message })); } catch { /* socket already closing */ }
    try { ws.close(1008, message); } catch { /* socket already closed */ }
  }

  findPeer(role) {
    const cached = this.peerCache.get(role);
    if (cached) {
      const attachment = cached.deserializeAttachment() || {};
      if (cached.readyState === 1 && !attachment.replaced && attachment.role === role) return cached;
      this.peerCache.delete(role);
    }
    const sockets = this.state.getWebSockets();
    const peer =
      sockets.find((s) => {
        const a = s.deserializeAttachment() || {};
        return a.role === role && !a.replaced && s.readyState === 1;
      }) || null;
    if (peer) this.peerCache.set(role, peer);
    return peer;
  }

  rememberPeer(role, ws) {
    this.peerCache.set(role, ws);
  }

  forgetPeer(ws) {
    const identity = ws.deserializeAttachment()?.sessionIdentity;
    if (identity) this.sessionChecks.delete(`${identity.gateHash}:${identity.authHash}`);
    for (const [key, peer] of this.peerCache) {
      if (peer === ws) this.peerCache.delete(key);
    }
  }
}

export class MediaRoom extends Room {}

const SIGNAL_MAX_BYTES = 512 * 1024;
const PENDING_MAX_BYTES = 512 * 1024;
const SESSION_CACHE_MS = 3_000;
const SESSION_CHECK_MS = 5_000;
const TICKET_TTL_MS = 12 * 60 * 60 * 1000;

function validSocketIdentity(identity) {
  return identity && /^[a-f0-9]{64}$/.test(identity.gateHash) && /^[a-f0-9]{64}$/.test(identity.authHash);
}

// A file-offer lists at most 64 items whose names are at most 1 KiB of
// UTF-8 each; JSON escaping can grow a name up to 6x.
const FILE_OFFER_MAX_RAW_BYTES = 256 + FT_MAX_BATCH_FILES * (FT_MAX_NAME_BYTES * 6 + 64);

function validFileDragPreview(msg, att, raw) {
  if (att.role !== "client" || typeof raw !== "string" || utf8Length(raw) > 8_192 ||
      msg.protocol !== 1 || !validCurrentMediaEpoch(msg.epoch, att) ||
      !Number.isInteger(msg.dragId) || msg.dragId < 1 || msg.dragId > 0xffff_ffff ||
      Object.keys(msg).some(key => !["type", "protocol", "epoch", "dragId", "action", "drop", "types"].includes(key))) return false;
  if (msg.action === "cancel") return !Object.hasOwn(msg, "drop") && !Object.hasOwn(msg, "types");
  if (msg.action !== "update" || !msg.drop || typeof msg.drop !== "object" || Array.isArray(msg.drop) ||
      Object.keys(msg.drop).length !== 4 ||
      Object.keys(msg.drop).some(key => !["x", "y", "generation", "viewportRevision"].includes(key))) return false;
  const integer = (value, min, max) => Number.isInteger(value) && value >= min && value <= max;
  return integer(msg.drop.x, 0, 0xffff) && integer(msg.drop.y, 0, 0xffff) &&
    integer(msg.drop.generation, 1, 0xffff_ffff) && integer(msg.drop.viewportRevision, 0, 0xffff_ffff) &&
    Array.isArray(msg.types) && msg.types.length >= 1 && msg.types.length <= 64 &&
    msg.types.every(mime => typeof mime === "string" && mime.length <= 96 && /^[\x21-\x7e]*$/.test(mime));
}

function binaryLength(value) {
  if (value instanceof ArrayBuffer) return value.byteLength;
  if (ArrayBuffer.isView(value)) return value.byteLength;
  return 0;
}

function randomEpoch() {
  const bytes = new Uint8Array(16); crypto.getRandomValues(bytes);
  return Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
}
