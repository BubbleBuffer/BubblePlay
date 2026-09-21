import { validWssTopology, validCurrentMediaEpoch, validMediaAckV3, validHostMediaCapabilities, validInputStatus, canonicalWorkerVersionId, validFirstFrameMessage, benchStatusRejectionReason, utf8Length } from "./protocol-validation.js";
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
import {
  LAB_EDGE_TRACE_BYTES,
  LAB_EDGE_TRACE_MAGIC,
  LAB_HOST_TRACE_BYTES,
  MAX_MEDIA_PACKET_BYTES,
  STREAM_LAB_COMPATIBILITY,
  V2_MAX_PACKET_BYTES,
  V3_MAX_PACKET_BYTES,
} from "../public/media.js";

export { ApprovalGate };

// The host's relay credential is separate from browser password authentication.

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
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
    const localLabBypass = isLocalLabSocket(url, env);
    if (isLocalStreamLabRequest(url, env) && ["GET", "HEAD"].includes(request.method) &&
        !isHostSocketPath(url.pathname) && !isBrowserSocketPath(url.pathname)) {
      return env.ASSETS.fetch(request);
    }
    if (isHostSocketPath(url.pathname)) {
      if (!localLabBypass && !(await authorized(request, env.HOST_GATE_TOKEN))) {
        return new Response("Not found", { status: 404, headers: { "Cache-Control": "no-store" } });
      }
      if (!validSocketRoute(url)) return badStripe();
      return handleWebSocket(request, env);
    }
    if (isBrowserSocketPath(url.pathname)) {
      if (!validSocketRoute(url)) return badStripe();
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
      return handleWebSocket(request, env);
    }
    return gate.fetch(request);
  },
};

function handleWebSocket(request, env) {
  // Room name bump = fresh Durable Object state. The previous "room" instance
  // accumulated stale sockets/buffers across several force-killed sessions
  // (phone attempt, UAC crash, test browsers), which poisoned the relay: a
  // stale offer from an old session was flushed to a new host, whose DTLS
  // certificate then didn't match ("remote fingerprint no match").
  const url = new URL(request.url);
  const media = isMediaSocketPath(url.pathname);
  const lab = isLabSocketPath(url.pathname);
  const namespace = media ? env.MEDIA_ROOM : env.ROOM;
  const epoch = url.searchParams.get("epoch");
  const stripe = url.searchParams.get("stripe");
  const run = url.searchParams.get("run");
  const mediaVersion = url.searchParams.get("v") || "1";
  const roomName = lab
    ? (media
      ? `lab-media-v${mediaVersion}:${run}:${epoch}:${stripe}`
      : `lab-signal-v1:${run}`)
    : (media && epoch ? `media-v${mediaVersion}:${epoch}:${stripe}` : media ? "media-v1" : "room-v3");
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
    pathname === "/host-version-ws";
}
function isBrowserSocketPath(pathname) {
  return pathname === "/ws" || pathname === "/media-ws" ||
    pathname === "/lab-ws" || pathname === "/lab-media-ws";
}
function isMediaSocketPath(pathname) { return pathname.endsWith("media-ws"); }
function isLabSocketPath(pathname) { return pathname.startsWith("/lab-"); }
function isLocalStreamLabRequest(url, env) {
  return env.STREAM_LAB_LOCAL_BYPASS === "1" &&
    ["127.0.0.1", "localhost", "[::1]"].includes(url.hostname);
}
function isLocalLabSocket(url, env) {
  return isLocalStreamLabRequest(url, env) && isLabSocketPath(url.pathname) && validSocketRoute(url);
}
function validSocketRoute(url) {
  if (isLabSocketPath(url.pathname) && !validLabRun(url)) return false;
  if (!isMediaSocketPath(url.pathname)) return true;
  if (!validStripe(url) || !validMediaSession(url)) return false;
  return !isLabSocketPath(url.pathname) || ["2", "3"].includes(url.searchParams.get("v"));
}
function validLabRun(url) {
  return /^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i
    .test(url.searchParams.get("run") || "") &&
    url.searchParams.get("compat") === STREAM_LAB_COMPATIBILITY.id;
}
function validStripe(url) {
  const value = url.searchParams.get("stripe") || "";
  return url.searchParams.get("v") === "3" ? /^(?:[0-9]|1[0-5])$/.test(value) : /^(0|1|2|3)$/.test(value);
}
function validMediaSession(url) {
  const version = url.searchParams.get("v");
  if (version === null || version === "1") return true;
  return (version === "2" || version === "3") && /^[a-f0-9]{32}$/.test(url.searchParams.get("epoch") || "");
}
function badStripe() { return new Response("Not found", { status: 404, headers: { "Cache-Control": "no-store" } }); }

export class Room {
  constructor(state, env) {
    this.state = state;
    this.env = env;
    // This cache is opportunistic only. Durable Object hibernation may create a
    // fresh Room at any time, so socket attachments remain authoritative and a
    // cache miss always rebuilds from state.getWebSockets().
    this.peerCache = new Map();
    this.mediaForwardTarget = new Map();
  }

  async fetch(request) {
    const pair = new WebSocketPair();
    const [client, server] = Object.values(pair);
    console.log("[room] socket accepted");

    // Remember the query token + role on the socket itself: with the
    // hibernation API the DO class is re-instantiated per wake, so instance
    // fields are not authoritative peer state. Attachments and storage are.
    const url = new URL(request.url);
    const media = isMediaSocketPath(url.pathname);
    const lab = isLabSocketPath(url.pathname);
    const localLab = isLocalLabSocket(url, this.env);
    const expectedRole = url.pathname === "/host-version-ws" ? "version-probe" :
      media ? (isHostSocketPath(url.pathname) ? "media-host" : "media-client") :
        (isHostSocketPath(url.pathname) ? "host" : "client");
    server.serializeAttachment({ role: null, expectedRole,
      stripe: media ? Number(url.searchParams.get("stripe")) : null,
      epoch: url.searchParams.get("epoch") || null,
      version: media ? Number(url.searchParams.get("v") || 1) : null,
      run: lab ? url.searchParams.get("run") : null,
      localLab });

    // acceptWebSocket() both accepts the server side and registers it for
    // hibernation-driven webSocketMessage/webSocketClose callbacks.
    this.state.acceptWebSocket(server);
    return new Response(null, { status: 101, webSocket: client });
  }

  async webSocketMessage(ws, message) {
    const att = ws.deserializeAttachment() || {};
    if (att.role === "media-host") {
      const limit = att.version === 3
        ? V3_MAX_PACKET_BYTES + (att.run ? LAB_HOST_TRACE_BYTES : 0)
        : att.epoch ? V2_MAX_PACKET_BYTES : MAX_MEDIA_PACKET_BYTES;
      if (binaryLength(message) <= limit && binaryLength(message) > 0) {
        const target = this.findPeer("media-client", att.stripe);
        if (target) {
          if (att.run) {
            const targetAttachment = target.deserializeAttachment() || {};
            const targetId = targetAttachment.registeredAtMs ?? "legacy";
            if (this.mediaForwardTarget.get(att.stripe) !== targetId) {
              this.mediaForwardTarget.set(att.stripe, targetId);
              console.log(`[room] media-forward run=${att.run} stripe=${att.stripe} target=${targetId} replaced=${!!targetAttachment.replaced}`);
              target.send(JSON.stringify({
                type: "media-forward",
                registeredAtMs: targetAttachment.registeredAtMs ?? null,
              }));
            }
          }
          target.send(att.run && att.version === 3 ? appendLabEdgeTrace(message) : message);
        }
      }
      return;
    }
    if (att.role === "media-client") return;
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
    try {
      msg = JSON.parse(message);
    } catch {
      return; // not JSON: ignore
    }

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

    // First message must be hello; assigns the socket a role.
    if (!att.role) {
      if (msg.type !== "hello") return;
      const configuredToken = this.env.TOKEN;
      const role = msg.role;
      if (!["host", "client", "media-host", "media-client", "version-probe"].includes(role) ||
          role !== att.expectedRole) {
        this.reject(ws, "bad role");
        return;
      }
      // Media-host already passed the separate HTTP bearer gate before this
      // request reached the room. Do not require or duplicate the relay token
      // inside the media socket hello.
      const tokenOk = role === "client" || role === "media-client" || role === "media-host" ||
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
      const existing = this.findPeer(role, att.stripe);
      const existingAttachment = existing?.deserializeAttachment?.() || {};
      console.log(`[room] hello role=${role} run=${att.run || "-"} stripe=${att.stripe ?? "-"} existing=${existing ? "yes" : "none"} existingTarget=${existingAttachment.registeredAtMs ?? "-"} existingReplaced=${!!existingAttachment.replaced}`);
      if (existing && existing !== ws) {
        // A replacement signaling host has no control or media state from the
        // old host. Force the live browser to establish a fresh signaling
        // epoch so capabilities, topology, and input bind together.
        if (role === "host") {
          const client = this.findPeer("client");
          try { client?.send(JSON.stringify({ type: "peer-left" })); } catch { /* client already gone */ }
        }
        this.reject(existing, `${role} replaced`);
      }
      // A fresh client means any buffered offer/ICE came from a previous,
      // now-dead session — delivering them to a future host would poison it
      // (the host accepts a stale offer, its DTLS times out, session dies).
      if (role === "client") {
        await this.state.storage.delete("pendingToHost");
      }
      const mediaEpoch = role === "client" ? randomEpoch() : null;
      const registeredAtMs = Date.now();
      ws.serializeAttachment(role === "client"
        ? { ...att, role, mediaEpoch, registeredAtMs }
        : { ...att, role, registeredAtMs });
      this.rememberPeer(role, att.stripe, ws);
      // Hosts keep the original wire shape. Clients receive only short-lived
      // TURN REST credentials, minted after authentication and never stored.
      const hello = role === "client"
        ? { type: "hello-ok", iceServers: await createTurnIceServers(this.env), mediaEpoch,
            ...(workerVersionId ? { workerVersionId } : {}) }
        : { type: "hello-ok",
            ...(role === "media-client" ? { registeredAtMs } : {}),
            ...(workerVersionId ? { workerVersionId } : {}) };
      ws.send(JSON.stringify(hello));
      console.log(`[room] ${role} registered run=${att.run || "-"} stripe=${att.stripe ?? "-"} target=${registeredAtMs}, hello-ok sent`);
      if (role === "client") {
        const hostCapabilities = this.findPeer("host")?.deserializeAttachment()?.hostCapabilities;
        if (typeof hostCapabilities === "string") ws.send(hostCapabilities);
      }

      // Flush anything the client sent before the host arrived.
      if (role === "host") {
        const pending = (await this.state.storage.get("pendingToHost")) || [];
        if (pending.length) {
          console.log(`[room] flushing ${pending.length} buffered msg(s) to host`);
          // The buffered messages came FROM the client and go TO the host —
          // which is this socket, `ws`.
          for (const m of pending) {
            ws.send(m);
          }
          await this.state.storage.delete("pendingToHost");
        }
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
    if (msg.type === "host-capabilities" &&
        (att.role !== "host" || typeof message !== "string" || utf8Length(message) > 512 ||
         typeof msg.serviceMode !== "boolean" || typeof msg.coldBoot !== "boolean" ||
         typeof msg.secureDesktop !== "boolean" || typeof msg.secureText !== "boolean" ||
         !validHostMediaCapabilities(msg))) {
      return;
    }
    if (msg.type === "host-capabilities") {
      ws.serializeAttachment({ ...att, hostCapabilities: message });
    }
    if (msg.type === "desktop-state" &&
        (att.role !== "host" || typeof message !== "string" || utf8Length(message) > 512 ||
         (msg.state !== "active" && msg.state !== "unavailable") ||
         !Number.isInteger(msg.generation) || typeof msg.reason !== "string" || msg.reason.length > 96 ||
         (msg.desktop !== undefined && msg.desktop !== "default" && msg.desktop !== "secure" && msg.desktop !== "none") ||
         (msg.input !== undefined && msg.input !== "enabled" && msg.input !== "suspended" && msg.input !== "read-only"))) {
      return;
    }
    if (msg.type === "input-status" && !validInputStatus(msg, att.role, message)) return;
    if (msg.type === "first-frame" && !validFirstFrameMessage(msg, att.role, message)) {
      return;
    }
    if ((msg.type === "media-control" || msg.type === "media-ack" || msg.type === "media-ack-v2" || msg.type === "media-ack-v3") &&
        (att.role !== "client" || typeof message !== "string" || utf8Length(message) > 2_048)) {
      return;
    }
    if (msg.type === "media-control" && msg.protocol === 3 && !validCurrentMediaEpoch(msg.epoch, att)) return;
    if (msg.type === "media-control" && msg.protocol === 3 && msg.action === "start" &&
        !validWssTopology(msg.topology)) return;
    if (msg.type === "media-ack-v3" && !validMediaAckV3(msg, att)) return;
    if (msg.type === "media-topology" &&
        (att.role !== "host" || typeof message !== "string" || utf8Length(message) > 1_024 ||
         msg.protocol !== 3 || !/^[a-f0-9]{32}$/.test(msg.epoch || "") ||
         (msg.status !== "accepted" && msg.status !== "rejected") || !validWssTopology(msg.topology) ||
         (msg.reason !== undefined && msg.reason !== null &&
          (typeof msg.reason !== "string" || msg.reason.length > 96)))) return;
    if (msg.type === "media-control" && msg.action === "monitor" &&
        (att.role !== "client" || typeof msg.monitor !== "string" || msg.monitor.length === 0 || msg.monitor.length > 128)) {
      return;
    }
    if (msg.type === "media-control" && msg.action === "monitors" &&
        (att.role !== "client" || (msg.protocol !== 2 && msg.protocol !== 3) || typeof msg.epoch !== "string" ||
         !/^[a-f0-9]{32}$/.test(msg.epoch) || msg.epoch !== att.mediaEpoch)) {
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
          (msg.type === "media-control" && (msg.action === "start" || msg.action === "monitors")) ||
          (msg.type === "first-frame" && msg.phase === "request"))) {
        const pending = (await this.state.storage.get("pendingToHost")) || [];
        if (msg.type === "media-control" || msg.type === "first-frame") {
          for (let i = pending.length - 1; i >= 0; i -= 1) {
            try {
              const old = JSON.parse(pending[i]);
              if ((msg.type === "media-control" && old.type === "media-control" && old.action === msg.action &&
                   (msg.action === "start" || msg.action === "monitors")) ||
                  (msg.type === "first-frame" && old.type === "first-frame" && old.phase === "request")) pending.splice(i, 1);
            } catch { /* retain validated offer/ICE wire text */ }
          }
        }
        pending.push(message);
        if (pending.length > 64) pending.shift();
        await this.state.storage.put("pendingToHost", pending);
      }
      return;
    }
    target.send(message);
  }

  async webSocketClose(ws, _code, _reason, _wasClean) {
    const att = ws.deserializeAttachment() || {};
    const role = att.role;
    this.forgetPeer(ws);
    console.log(`[room] socket closed role=${role || "unassigned"} run=${att.run || "-"} stripe=${att.stripe ?? "-"} target=${att.registeredAtMs ?? "-"} replaced=${!!att.replaced}`);
    if (!role) return;
    // When the client dies, its buffered offer/ICE is worthless — clear it so
    // a later host never receives a stale offer from a dead peer.
    if (role === "client") {
      await this.state.storage.delete("pendingToHost");
    }
    if (att.replaced) return; // replaced sockets get no farewell
    // Media stripe churn is isolated from the signaling session. Losing one
    // TCP lane must not tell the host/browser that the application peer left.
    if (role === "media-host" || role === "media-client") return;
    const peer = role === "host" ? this.findPeer("client") : this.findPeer("host");
    if (peer) {
      try {
        peer.send(JSON.stringify({ type: "peer-left" }));
      } catch {
        // peer already gone
      }
    }
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

  findPeer(role, stripe = null) {
    const key = this.peerCacheKey(role, stripe);
    const cached = this.peerCache.get(key);
    if (cached) {
      const attachment = cached.deserializeAttachment() || {};
      if (cached.readyState === 1 && !attachment.replaced &&
          attachment.role === role && (attachment.stripe ?? null) === stripe) return cached;
      this.peerCache.delete(key);
    }
    const sockets = this.state.getWebSockets();
    const peer =
      sockets.find((s) => {
        const a = s.deserializeAttachment() || {};
        return a.role === role && (a.stripe ?? null) === stripe && !a.replaced && s.readyState === 1;
      }) || null;
    if (peer) this.peerCache.set(key, peer);
    return peer;
  }

  rememberPeer(role, stripe, ws) {
    this.peerCache.set(this.peerCacheKey(role, stripe), ws);
  }

  forgetPeer(ws) {
    for (const [key, peer] of this.peerCache) {
      if (peer === ws) this.peerCache.delete(key);
    }
  }

  peerCacheKey(role, stripe) {
    return `${role}:${stripe ?? ""}`;
  }
}

export class MediaRoom extends Room {}

function binaryLength(value) {
  if (value instanceof ArrayBuffer) return value.byteLength;
  if (ArrayBuffer.isView(value)) return value.byteLength;
  return 0;
}

function appendLabEdgeTrace(message, edgeReceivedAtEpochMs = Date.now()) {
  const source = message instanceof ArrayBuffer
    ? new Uint8Array(message)
    : ArrayBuffer.isView(message)
      ? new Uint8Array(message.buffer, message.byteOffset, message.byteLength)
      : null;
  if (!source) return message;
  const tagged = new Uint8Array(source.byteLength + LAB_EDGE_TRACE_BYTES);
  tagged.set(source);
  const trailer = new DataView(tagged.buffer, source.byteLength, LAB_EDGE_TRACE_BYTES);
  trailer.setUint32(0, LAB_EDGE_TRACE_MAGIC);
  trailer.setFloat64(4, edgeReceivedAtEpochMs);
  return tagged.buffer;
}

function randomEpoch() {
  const bytes = new Uint8Array(16); crypto.getRandomValues(bytes);
  return Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
}
