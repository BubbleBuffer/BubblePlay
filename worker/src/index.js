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

function validWssTopology(topology) {
  if (!topology || typeof topology !== "object" || Array.isArray(topology)) return false;
  if (Object.keys(topology).some(key => !["laneCount", "dataShards", "parityShards"].includes(key))) return false;
  const { laneCount, dataShards, parityShards } = topology;
  return Number.isInteger(laneCount) && laneCount >= 1 && laneCount <= 16 &&
    Number.isInteger(dataShards) && dataShards >= 1 && dataShards <= 16 &&
    Number.isInteger(parityShards) && parityShards >= 0 && parityShards <= 2 &&
    laneCount === dataShards + parityShards;
}

function validCurrentMediaEpoch(epoch, attachment) {
  return typeof epoch === "string" && /^[a-f0-9]{32}$/.test(epoch) && epoch === attachment.mediaEpoch;
}

function uint32(value) { return Number.isInteger(value) && value >= 0 && value <= 0xffff_ffff; }

function validMediaAckV3(message, attachment) {
  if (message.protocol !== 3 || !validCurrentMediaEpoch(message.epoch, attachment) ||
      !validWssTopology(message.topology) || !Array.isArray(message.lanes) ||
      message.lanes.length !== message.topology.laneCount || !message.lanes.every(uint32)) return false;
  if (message.completionMissingByLane !== undefined &&
      (!Array.isArray(message.completionMissingByLane) ||
       message.completionMissingByLane.length !== message.topology.laneCount ||
       !message.completionMissingByLane.every(uint32))) return false;
  for (const field of ["generation", "reconstructed", "presented", "recoveredFrames", "expiredFrames",
    "decoderDrops", "dependencyLosses", "deadlineMs", "reassemblyP95Ms"]) if (!uint32(message[field])) return false;
  return ["healthy", "awaiting-keyframe", "awaiting-keyframe-output"].includes(message.recoveryState);
}

function validHostMediaCapabilities(message) {
  const fields = [message.mediaProtocols, message.maxWssLanes, message.wssFec];
  if (fields.every(value => value === undefined)) return true;
  const validFec = Array.isArray(message.wssFec) &&
    (message.wssFec.length === 2 || message.wssFec.length === 3) &&
    message.wssFec[0] === "none" && message.wssFec[1] === "xor-1" &&
    (message.wssFec.length === 2 || message.wssFec[2] === "rs-2");
  return Array.isArray(message.mediaProtocols) && message.mediaProtocols.length === 2 &&
    message.mediaProtocols[0] === 2 && message.mediaProtocols[1] === 3 &&
    message.maxWssLanes === 16 && validFec;
}

function validInputStatus(message, role, raw) {
  if (role !== "host" || typeof raw !== "string" || utf8Length(raw) > 1_024 ||
      message.protocol !== 1 || !uint32(message.generation)) return false;
  const counters = ["receivedPackets", "acceptedPackets", "failedPackets", "droppedPackets",
    "releasedInputs", "requestedEvents", "appliedEvents"];
  if (!counters.every(field => Number.isSafeInteger(message[field]) && message[field] >= 0)) return false;
  if (message.acceptedPackets > message.receivedPackets ||
      message.failedPackets > message.receivedPackets ||
      message.droppedPackets > message.receivedPackets ||
      message.appliedEvents > message.requestedEvents) return false;
  if (message.lastErrorCode !== undefined &&
      (!Number.isInteger(message.lastErrorCode) || message.lastErrorCode < -0x8000_0000 ||
       message.lastErrorCode > 0x7fff_ffff)) return false;
  return message.lastSequence === undefined ||
    (Number.isInteger(message.lastSequence) && message.lastSequence >= 0 && message.lastSequence <= 0xffff);
}

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

function canonicalWorkerVersionId(value) {
  return typeof value === "string" &&
    /^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(value);
}

function validFirstFrameMessage(msg, role, raw) {
  if (typeof raw !== "string" || utf8Length(raw) > 1_024 || msg.protocol !== 1 ||
      typeof msg.requestId !== "string" || !/^[A-Za-z0-9_-]{8,64}$/.test(msg.requestId) ||
      !["request", "admitted", "presented", "ready", "failed", "quality-keyframe-admitted"].includes(msg.phase)) return false;
  if (msg.phase === "quality-keyframe-admitted") {
    return role === "host" &&
      /^quality-[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(msg.requestId) &&
      msg.transport === "wss" &&
      Number.isInteger(msg.generation) && msg.generation >= 0 && msg.generation <= 0xffffffff &&
      Number.isInteger(msg.sequence) && msg.sequence >= 0 && msg.sequence <= 0xffffffff &&
      ["hevc", "h264"].includes(msg.codec) &&
      msg.reason === "stream-lab-quality-capture";
  }
  if (msg.phase === "request") {
    if (role !== "client" || !["webrtc", "wss"].includes(msg.transport)) return false;
    if (msg.epoch !== undefined && (typeof msg.epoch !== "string" || !/^[a-f0-9]{32}$/.test(msg.epoch))) return false;
    return true;
  }
  if (msg.phase === "presented") {
    return role === "client" && Number.isInteger(msg.generation) && msg.generation >= 0 && msg.generation <= 0xffffffff;
  }
  if (role !== "host" || !Number.isInteger(msg.generation) || msg.generation < 0 || msg.generation > 0xffffffff) return false;
  if (msg.phase === "failed") return typeof msg.reason === "string" && msg.reason.length > 0 && msg.reason.length <= 96;
  return msg.phase === "admitted" || msg.phase === "ready";
}

export function benchStatusRejectionReason(msg, role, raw) {
  const encoder = msg?.effective?.encoder;
  const expandedRoiMode = encoder?.roiMode;
  const normalizedRoiMode = expandedRoiMode ?? (encoder?.dirtyQpMap === true ? "damage" : "off");
  if (role !== "host" || typeof raw !== "string" || utf8Length(raw) > 4_096)
    return "role-or-wire";
  if (![1, 2].includes(msg.protocol)) return "protocol";
  if (!/^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(msg.runId || ""))
    return "run-id";
  if (!Number.isSafeInteger(msg.sequence) || msg.sequence < 0 ||
      !Number.isSafeInteger(msg.profileRevision) || msg.profileRevision < 0)
    return "sequence-or-revision";
  if (!["applied", "sample", "reverted", "error"].includes(msg.phase)) return "phase";
  if (msg.protocol !== 1 && !validBenchExperiment(msg)) return "experiment";
  if (!validBenchEffective(msg.effective, msg.protocol)) return "effective-profile";
  if (msg.protocol !== 1 && (msg.effective.implementationVariant !== msg.implementationVariant ||
      msg.effective.experimentId !== msg.experimentId))
    return "experiment-binding";
  if (msg.roiActivity !== undefined &&
      !(msg.protocol === 2 && normalizedRoiMode !== "off" &&
        validBenchRoiActivity(msg.roiActivity, normalizedRoiMode)))
    return "roi-activity";
  if (expandedRoiMode !== undefined && normalizedRoiMode !== "off" && msg.roiActivity === undefined)
    return "roi-activity-required";
  if (msg.encodeStages !== undefined &&
      !(msg.protocol === 2 && validBenchEncodeStages(msg.encodeStages)))
    return "encode-stages";
  if (msg.hybridActivity === undefined &&
      ["png-quiescent-detail", "hevc-quiescent-tiles"].includes(msg.effective?.hybridMode))
    return "hybrid-activity-required";
  if (msg.hybridActivity !== undefined &&
      !(msg.protocol === 2 && validBenchHybridActivity(msg.hybridActivity, msg.effective?.hybridMode)))
    return "hybrid-activity";
  if (msg.metrics !== undefined &&
      (msg.metrics === null || typeof msg.metrics !== "object" || Array.isArray(msg.metrics)))
    return "metrics";
  return null;
}

function validBenchStatus(msg, role, raw) {
  return benchStatusRejectionReason(msg, role, raw) === null;
}

function validBenchRoiActivity(value, expectedMode) {
  const required = ["mode", "evaluatedFrames", "mappedFrames", "missingDamageFrames", "emptyDamageFrames",
    "idrFrames", "coverageRejectedFrames", "unsupportedFrames"];
  const optional = ["source", "temporalFrames", "temporalPrimingFrames", "analysisP95Us", "analysisMaxUs",
    "lastSequence", "lastReason", "lastChangedCtbs", "lastTotalCtbs", "lastMinDelta", "lastMaxDelta",
    "reconstructionActive", "feedbackSubmittedFrames", "feedbackReadyFrames", "feedbackLateFrames",
    "feedbackSkippedFrames", "feedbackAdjustedFrames", "feedbackWarmupFrames",
    "feedbackMotionBypasses", "feedbackMissingFrames", "feedbackEligibleCtbs",
    "feedbackRefinedCtbs", "feedbackFundedCtbs", "feedbackSubmitP95Us",
    "feedbackSubmitMaxUs", "feedbackReadbackP95Us", "feedbackReadbackMaxUs",
    "lastFeedbackSequence", "lastFeedbackAgeFrames", "feedbackSpendHeadroomFrames",
    "feedbackNetZeroFrames", "feedbackWireOccupancyP95Bps", "feedbackWireOccupancyMaxBps",
    "lastFeedbackBudgetPolicy", "lastFeedbackWireOccupancyBps"];
  if (value === null || typeof value !== "object" || Array.isArray(value) ||
      !required.every(key => Object.hasOwn(value, key)) ||
      !Object.keys(value).every(key => required.includes(key) || optional.includes(key)) ||
      value.mode !== expectedMode) return false;
  const countKeys = required.slice(1);
  if (!countKeys.every(key => Number.isSafeInteger(value[key]) && value[key] >= 0)) return false;
  if (value.evaluatedFrames !== countKeys.slice(1).reduce((sum, key) => sum + value[key], 0)) return false;
  if (value.lastSequence !== undefined && !boundedInteger(value.lastSequence, 0, 0xffff_ffff)) return false;
  if (value.lastReason !== undefined && !["applied", "missing-damage", "empty-damage", "idr",
    "coverage-at-least-half", "unsupported-encoder-state"].includes(value.lastReason)) return false;
  if (value.lastChangedCtbs !== undefined && !boundedInteger(value.lastChangedCtbs, 0, 8_160)) return false;
  if (value.lastTotalCtbs !== undefined && !boundedInteger(value.lastTotalCtbs, 0, 8_160)) return false;
  if (value.lastChangedCtbs !== undefined && value.lastTotalCtbs !== undefined &&
      value.lastChangedCtbs > value.lastTotalCtbs) return false;
  if (value.lastMinDelta !== undefined && !boundedInteger(value.lastMinDelta, -5, 2)) return false;
  if (value.lastMaxDelta !== undefined && !boundedInteger(value.lastMaxDelta, -5, 2)) return false;
  if (value.source !== undefined && !["dda", "gpu-temporal"].includes(value.source)) return false;
  if (value.temporalFrames !== undefined && !boundedInteger(value.temporalFrames, 0, 1_000_000)) return false;
  if (value.temporalPrimingFrames !== undefined &&
      !boundedInteger(value.temporalPrimingFrames, 0, 1_000_000)) return false;
  if (value.temporalPrimingFrames !== undefined && value.temporalFrames !== undefined &&
      value.temporalPrimingFrames > value.temporalFrames) return false;
  if (value.analysisP95Us !== undefined && !boundedInteger(value.analysisP95Us, 0, 60_000_000)) return false;
  if (value.analysisMaxUs !== undefined && !boundedInteger(value.analysisMaxUs, 0, 60_000_000)) return false;
  if (value.analysisP95Us !== undefined && value.analysisMaxUs !== undefined &&
      value.analysisP95Us > value.analysisMaxUs) return false;
  const temporalKeysPresent = ["temporalFrames", "temporalPrimingFrames", "analysisP95Us", "analysisMaxUs"]
    .some(key => value[key] !== undefined);
  if (temporalKeysPresent && value.source !== "gpu-temporal") return false;
  if (value.source === "gpu-temporal" &&
      (!Number.isSafeInteger(value.temporalFrames) || value.temporalFrames <= 0 ||
       value.analysisP95Us === undefined || value.analysisMaxUs === undefined)) return false;
  const feedbackCountKeys = ["feedbackSubmittedFrames", "feedbackReadyFrames", "feedbackLateFrames",
    "feedbackSkippedFrames", "feedbackAdjustedFrames", "feedbackWarmupFrames",
    "feedbackMotionBypasses", "feedbackMissingFrames", "feedbackEligibleCtbs",
    "feedbackRefinedCtbs", "feedbackFundedCtbs"];
  const feedbackTimingKeys = ["feedbackSubmitP95Us", "feedbackSubmitMaxUs",
    "feedbackReadbackP95Us", "feedbackReadbackMaxUs"];
  const feedbackKeys = ["reconstructionActive", ...feedbackCountKeys, ...feedbackTimingKeys];
  const feedbackLastKeys = ["lastFeedbackSequence", "lastFeedbackAgeFrames"];
  const feedbackMode = ["feedback", "feedback-headroom"].includes(expectedMode);
  const feedbackKeysPresent = [...feedbackKeys, ...feedbackLastKeys]
    .some(key => value[key] !== undefined);
  if (feedbackKeysPresent !== feedbackMode) return false;
  if (feedbackMode) {
    if (!feedbackKeys.every(key => Object.hasOwn(value, key)) ||
        typeof value.reconstructionActive !== "boolean")
      return false;
    if (!feedbackCountKeys.every(key => boundedInteger(value[key], 0, 1_000_000))) return false;
    if (!feedbackTimingKeys.every(key => boundedInteger(value[key], 0, 60_000_000))) return false;
    const lastKeysPresent = feedbackLastKeys.filter(key => value[key] !== undefined).length;
    if (lastKeysPresent !== 0 && lastKeysPresent !== feedbackLastKeys.length) return false;
    if (lastKeysPresent && (!boundedInteger(value.lastFeedbackSequence, 0, 0xffff_ffff) ||
        value.lastFeedbackAgeFrames !== 1)) return false;
    if (value.feedbackAdjustedFrames !== value.mappedFrames ||
        value.feedbackSubmitP95Us > value.feedbackSubmitMaxUs ||
        value.feedbackReadbackP95Us > value.feedbackReadbackMaxUs) return false;
    if (expectedMode === "feedback" && value.feedbackFundedCtbs !== value.feedbackRefinedCtbs * 2)
      return false;
  }
  const headroomCountKeys = ["feedbackSpendHeadroomFrames", "feedbackNetZeroFrames"];
  const headroomOccupancyKeys = ["feedbackWireOccupancyP95Bps", "feedbackWireOccupancyMaxBps"];
  const headroomLastKeys = ["lastFeedbackBudgetPolicy", "lastFeedbackWireOccupancyBps"];
  const headroomKeys = [...headroomCountKeys, ...headroomOccupancyKeys, ...headroomLastKeys];
  const headroomKeysPresent = headroomKeys.filter(key => value[key] !== undefined).length;
  if (expectedMode === "feedback-headroom") {
    if (![headroomKeys.length - headroomLastKeys.length, headroomKeys.length].includes(headroomKeysPresent) ||
        !headroomCountKeys.every(key => boundedInteger(value[key], 0, 1_000_000)) ||
        !headroomOccupancyKeys.every(key => boundedInteger(value[key], 0, 1_000_000_000)) ||
        value.feedbackSpendHeadroomFrames + value.feedbackNetZeroFrames !== value.feedbackAdjustedFrames ||
        value.feedbackWireOccupancyP95Bps > value.feedbackWireOccupancyMaxBps)
      return false;
    const headroomLastPresent = headroomLastKeys.filter(key => value[key] !== undefined).length;
    if (headroomLastPresent !== 0 && headroomLastPresent !== headroomLastKeys.length) return false;
    if (headroomLastPresent &&
        (!['net-zero', 'spend-headroom'].includes(value.lastFeedbackBudgetPolicy) ||
         !boundedInteger(value.lastFeedbackWireOccupancyBps, 0, 1_000_000_000))) return false;
  } else if (headroomKeysPresent !== 0) {
    return false;
  }
  return (value.lastMinDelta === undefined || value.lastMaxDelta === undefined ||
    value.lastMinDelta <= value.lastMaxDelta);
}

function validBenchEncodeStages(value) {
  const stageNames = ["register", "map", "submit", "lock", "copy", "unmap", "unregister"];
  if (!exactObjectKeys(value, ["frames", ...stageNames]) ||
      !Number.isSafeInteger(value.frames) || value.frames < 0) return false;
  for (const name of stageNames) {
    const stage = value[name];
    if (!exactObjectKeys(stage, ["count", "totalUs", "maxUs"]) ||
        !Number.isSafeInteger(stage.count) || stage.count < 0 ||
        !Number.isSafeInteger(stage.totalUs) || stage.totalUs < 0 ||
        !Number.isSafeInteger(stage.maxUs) || stage.maxUs < 0 ||
        stage.maxUs > stage.totalUs ||
        (stage.count === 0 && (stage.totalUs !== 0 || stage.maxUs !== 0))) return false;
  }
  if (!["map", "submit", "lock", "copy", "unmap"].every(name => value[name].count === value.frames))
    return false;
  return value.register.count <= value.frames && value.unregister.count <= value.frames;
}

function validBenchHybridActivity(value, expectedMode) {
  const countKeys = ["frames", "historyResets", "historyPreservedFrames", "motionBypasses",
    "cropRemaps", "cursorNeutralFrames", "candidates", "reissuedTiles", "upserts",
    "invalidations", "budgetSkippedUpserts", "pngBytes", "pngUsTotal", "pngUsMax",
    "sidecarBytes", "deferredValidationReadyFrames", "deferredValidationPendingFrames",
    "damageAuditFrames", "damageAuditMetadataUnavailableFrames", "damageAuditChangedCtbs",
    "damageAuditCoveredCtbs", "damageAuditMissedCtbs"];
  const validationCounts = ["validationFrames", "validationUnavailableFrames", "validationFailedFrames"];
  const optionalZeroCounts = ["validationUnavailableFrames", "validationFailedFrames",
    "damageAuditMetadataUnavailableFrames", "damageAuditMissedCtbs"];
  const validationTimings = ["validationSubmitP95Us", "validationSubmitMaxUs",
    "validationCollectP95Us", "validationCollectMaxUs"];
  const deferredAges = ["deferredValidationAgeP95Frames", "deferredValidationAgeMaxFrames",
    "deferredValidationPendingMaxAgeFrames"];
  const hevcTileCounts = ["hevcTileBytes", "hevcTileCopyUsTotal", "hevcTileCopyUsMax",
    "hevcTileEncodeUsTotal", "hevcTileEncodeUsMax", "hevcTileEncoderOpens",
    "hevcTileEncoderFailures"];
  const optional = ["analysisP95Us", "analysisMaxUs", ...validationCounts, ...validationTimings,
    "lastValidationSequence", ...deferredAges, ...hevcTileCounts];
  if (!exactObjectKeys(value, ["mode", "cache",
    ...countKeys.filter(key => !optionalZeroCounts.includes(key)),
    ...optionalZeroCounts.filter(key => value?.[key] !== undefined),
    ...optional.filter(key => !optionalZeroCounts.includes(key) && value?.[key] !== undefined)]) ||
      !["png-quiescent-detail", "hevc-quiescent-tiles"].includes(expectedMode) ||
      value.mode !== expectedMode ||
      !countKeys.every(key => Number.isSafeInteger(value[key] ?? 0) && (value[key] ?? 0) >= 0)) return false;
  if (!hevcTileCounts.every(key => value[key] === undefined ||
      (Number.isSafeInteger(value[key]) && value[key] >= 0))) return false;
  if (!exactObjectKeys(value.cache, ["s", "r", "i", "x", "b", "a"]) ||
      !["s", "r", "i", "x", "b", "a"].every(
        key => Number.isSafeInteger(value.cache[key]) && value.cache[key] >= 0)) return false;
  if (value.pngUsMax > value.pngUsTotal) return false;
  if ((value.hevcTileCopyUsMax ?? 0) > (value.hevcTileCopyUsTotal ?? 0) ||
      (value.hevcTileEncodeUsMax ?? 0) > (value.hevcTileEncodeUsTotal ?? 0)) return false;
  if (value.analysisP95Us !== undefined &&
      !boundedInteger(value.analysisP95Us, 0, 60_000_000)) return false;
  if (value.analysisMaxUs !== undefined &&
      !boundedInteger(value.analysisMaxUs, 0, 60_000_000)) return false;
  if (value.analysisP95Us !== undefined && value.analysisMaxUs !== undefined &&
      value.analysisP95Us > value.analysisMaxUs) return false;
  const validationPresent = optional.slice(2).some(key => value[key] !== undefined);
  if (!validationPresent) return true;
  if (!validationCounts.every(key => boundedInteger(value[key] ?? 0, 0, 1_000_000))) return false;
  if ((value.validationUnavailableFrames ?? 0) + (value.validationFailedFrames ?? 0) >
      value.validationFrames)
    return false;
  if (value.lastValidationSequence !== undefined &&
      !boundedInteger(value.lastValidationSequence, 0, 0xffff_ffff)) return false;
  for (const key of validationTimings) {
    if (value[key] !== undefined && !boundedInteger(value[key], 0, 60_000_000)) return false;
  }
  if (value.validationFrames > 0 && !validationTimings.every(key => value[key] !== undefined))
    return false;
  if (!deferredAges.every(key => value[key] === undefined || boundedInteger(value[key], 0, 1_000_000)))
    return false;
  if (value.deferredValidationAgeP95Frames !== undefined &&
      value.deferredValidationAgeMaxFrames !== undefined &&
      value.deferredValidationAgeP95Frames > value.deferredValidationAgeMaxFrames) return false;
  if ((value.damageAuditMetadataUnavailableFrames ?? 0) > value.damageAuditFrames ||
      value.damageAuditCoveredCtbs > value.damageAuditChangedCtbs ||
      (value.damageAuditMissedCtbs ?? 0) > value.damageAuditChangedCtbs ||
      value.damageAuditCoveredCtbs + (value.damageAuditMissedCtbs ?? 0) !==
        value.damageAuditChangedCtbs)
    return false;
  return (value.validationSubmitP95Us === undefined || value.validationSubmitMaxUs === undefined ||
      value.validationSubmitP95Us <= value.validationSubmitMaxUs) &&
    (value.validationCollectP95Us === undefined || value.validationCollectMaxUs === undefined ||
      value.validationCollectP95Us <= value.validationCollectMaxUs);
}

function validBenchExperiment(value) {
  return ["production", "candidate"].includes(value.implementationVariant) &&
    typeof value.experimentId === "string" && /^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$/.test(value.experimentId);
}

function validBenchEffective(effective, protocol = 1) {
  const keys = ["transport", "codec", "streamMode", "width", "height", "fps", "bitrateBps", "gopSeconds", "encoder"];
  if (protocol === 2) {
    keys.push("implementationVariant", "experimentId");
    if (effective?.wssTopology !== undefined) keys.push("wssTopology");
    if (effective?.hybridMode !== undefined) keys.push("hybridMode");
    if (effective?.hybridCacheMode !== undefined) keys.push("hybridCacheMode");
  }
  if (!exactObjectKeys(effective, keys)) return false;
  const maxFps = effective.streamMode === "work" ? 30 : effective.streamMode === "game" ? 60 : 0;
  // The v2 experiment search remains 2–12 Mbps, while a shaped lab unit may
  // publish a lower effective target so parity and framing fit under its
  // total browser-delivered media cap.
  const bitrateBounds = protocol === 2 ? [500_000, 12_000_000] : [500_000, 40_000_000];
  return ["wss", "webrtc"].includes(effective.transport) && effective.codec === "hevc" && maxFps > 0 &&
    boundedInteger(effective.width, 320, 3_840) && boundedInteger(effective.height, 180, 2_160) &&
    boundedInteger(effective.fps, 1, maxFps) && boundedInteger(effective.bitrateBps, ...bitrateBounds) &&
    [1, 2, 4].includes(effective.gopSeconds) && (protocol === 1 || validBenchExperiment(effective)) &&
    (effective.wssTopology === undefined || (effective.transport === "wss" && validWssTopology(effective.wssTopology))) &&
    validBenchEncoder(effective.encoder, protocol, effective.streamMode) &&
    validBenchPipelineControls(effective, protocol);
}

function validBenchPipelineControls(effective, protocol) {
  if (protocol !== 2) return true;
  const encoder = effective.encoder;
  const hybridCacheMode = effective.hybridCacheMode ?? "suppress-reactivate";
  const expanded = effective.hybridMode !== undefined || encoder.roiMode !== undefined ||
    encoder.resourceMode !== undefined || encoder.asyncDepth !== undefined ||
    effective.hybridCacheMode !== undefined || encoder.rateControlMode !== undefined;
  if (!expanded) return true;
  if (!["off", "png-static-roi", "png-quiescent-detail", "hevc-quiescent-tiles"].includes(
    effective.hybridMode) ||
      !["dda-gated", "suppress-reactivate"].includes(hybridCacheMode) ||
      !["off", "damage", "text-edge", "feedback", "feedback-headroom"].includes(encoder.roiMode) ||
      !["per-frame", "persistent"].includes(encoder.resourceMode) ||
      ![0, 1].includes(encoder.asyncDepth)) return false;
  const rateControlMode = encoder.rateControlMode ?? "cbr";
  if (!["cbr", "external-budget-vbr"].includes(rateControlMode) ||
      (rateControlMode === "cbr" &&
       (encoder.targetQuality !== undefined || encoder.peakBitrateMultiplier !== undefined)) ||
      (rateControlMode === "external-budget-vbr" &&
       (encoder.targetQuality !== 23 || encoder.peakBitrateMultiplier !== 4 || encoder.vbvFrames !== 4)))
    return false;
  if (hybridCacheMode === "dda-gated" && effective.hybridMode !== "png-quiescent-detail")
    return false;
  const activeAxes = Number(encoder.roiMode !== "off") + Number(effective.hybridMode !== "off") +
    Number(encoder.resourceMode !== "per-frame" || encoder.asyncDepth !== 0) +
    Number(rateControlMode !== "cbr");
  if (activeAxes === 0) return true;
  const topology = effective.wssTopology;
  return activeAxes === 1 && effective.implementationVariant === "candidate" &&
    effective.transport === "wss" && effective.streamMode === "work" &&
    effective.width === 1920 && effective.height === 1080 && effective.fps === 30 &&
    boundedInteger(effective.bitrateBps, 500_000, 6_000_000) &&
    topology?.laneCount === 16 && topology.dataShards === 15 && topology.parityShards === 1 &&
    encoder.main10 === false && (encoder.asyncDepth === 0 || encoder.resourceMode === "persistent");
}

function validBenchEncoder(encoder, protocol = 1, streamMode = "work") {
  if (protocol === 2) {
    const legacyKeys = ["preset", "tuning", "multipass", "aqMode", "aqStrength", "vbvFrames", "lookaheadDepth", "weightedPrediction", "dirtyQpMap", "main10"];
    const expandedKeys = [...legacyKeys, "roiMode", "resourceMode", "asyncDepth"];
    const rateControlKeys = [...expandedKeys, "rateControlMode", "targetQuality", "peakBitrateMultiplier"];
    const shapeValid = exactObjectKeys(encoder, legacyKeys) || exactObjectKeys(encoder, expandedKeys) ||
      exactObjectKeys(encoder, rateControlKeys);
    return shapeValid &&
      ["p1", "p2", "p3", "p4", "p5", "p6", "p7"].includes(encoder.preset) &&
      ["ultra-low-latency", "low-latency"].includes(encoder.tuning) &&
      ["disabled", "quarter", "full"].includes(encoder.multipass) &&
      ["none", "spatial", "temporal"].includes(encoder.aqMode) &&
      boundedInteger(encoder.aqStrength, 1, 15) && boundedInteger(encoder.vbvFrames, 1, 4) &&
      boundedInteger(encoder.lookaheadDepth, 0, streamMode === "game" ? 1 : 2) &&
      typeof encoder.weightedPrediction === "boolean" && typeof encoder.dirtyQpMap === "boolean" &&
      typeof encoder.main10 === "boolean" &&
      (encoder.roiMode === undefined || ["off", "damage", "text-edge", "feedback", "feedback-headroom"].includes(encoder.roiMode)) &&
      (encoder.resourceMode === undefined || ["per-frame", "persistent"].includes(encoder.resourceMode)) &&
      (encoder.asyncDepth === undefined || [0, 1].includes(encoder.asyncDepth)) &&
      (encoder.rateControlMode === undefined || encoder.rateControlMode === "external-budget-vbr") &&
      (encoder.rateControlMode === undefined ||
       (encoder.targetQuality === 23 && encoder.peakBitrateMultiplier === 4 && encoder.vbvFrames === 4)) &&
      !(encoder.dirtyQpMap && encoder.roiMode !== undefined && encoder.roiMode !== "damage");
  }
  return exactObjectKeys(encoder, ["preset", "tuning", "multipass", "aqStrength", "temporalAq", "vbvFrames"]) &&
    ["p1", "p3", "p4", "p5"].includes(encoder.preset) &&
    ["ultra-low-latency", "low-latency"].includes(encoder.tuning) &&
    ["disabled", "quarter"].includes(encoder.multipass) &&
    boundedInteger(encoder.aqStrength, 1, 15) && typeof encoder.temporalAq === "boolean" &&
    boundedInteger(encoder.vbvFrames, 1, 4);
}

function exactObjectKeys(value, expected) {
  if (value === null || typeof value !== "object" || Array.isArray(value)) return false;
  const keys = Object.keys(value);
  return keys.length === expected.length && expected.every(key => Object.hasOwn(value, key));
}

function boundedInteger(value, minimum, maximum) {
  return Number.isInteger(value) && value >= minimum && value <= maximum;
}

// A distinct Durable Object instance keeps high-frequency media forwarding
// off the signaling actor. The protocol implementation is shared, but storage,
// socket membership, hibernation and failure lifecycle are isolated.
export class MediaRoom extends Room {}

function utf8Length(value) {
  return new TextEncoder().encode(value).byteLength;
}

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
