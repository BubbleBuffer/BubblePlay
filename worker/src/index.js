// BubblePlay signaling relay.
//
// One Cloudflare Worker doing two jobs:
//   1. Static hosting of the client page (worker/public/) via Workers Assets.
//   2. A WebSocket relay pairing exactly one host with one client.
//
// Wire protocol (JSON text frames), shared with host/src/signaling.rs:
//   any    -> worker : {type:"hello", token, role:"host"|"client"}
//   worker -> any    : {type:"hello-ok"} | {type:"error", message}
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
// Token auth: `hello.token` must equal the worker secret `TOKEN`. The client
// page also accepts ?token= in the URL to prefill its input.

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    if (url.pathname === "/ws") {
      return handleWebSocket(request, env);
    }
    // Everything else: the client page (index.html) and any static files.
    const response = await env.ASSETS.fetch(request);
    // The client page changes fast during bring-up; never let the browser or
    // the edge cache it, or phones keep running the stale pre-fix version
    // (which is exactly the blackscreen the user saw).
    const headers = new Headers(response.headers);
    headers.set("Cache-Control", "no-store");
    return new Response(response.body, {
      status: response.status,
      statusText: response.statusText,
      headers,
    });
  },
};

function handleWebSocket(request, env) {
  // Room name bump = fresh Durable Object state. The previous "room" instance
  // accumulated stale sockets/buffers across several force-killed sessions
  // (phone attempt, UAC crash, test browsers), which poisoned the relay: a
  // stale offer from an old session was flushed to a new host, whose DTLS
  // certificate then didn't match ("remote fingerprint no match").
  const roomId = env.ROOM.idFromName("room-v3");
  const stub = env.ROOM.get(roomId);
  // Pass the ORIGINAL request straight through. Reconstructing headers here
  // strips the hop-by-hop `Upgrade: websocket` header, and the DO then rejects
  // the 101 WebSocket response ("request did not contain Upgrade: websocket").
  return stub.fetch(request);
}

export class Room {
  constructor(state, env) {
    this.state = state;
    this.env = env;
  }

  async fetch(request) {
    const queryToken = new URL(request.url).searchParams.get("token") || "";
    const pair = new WebSocketPair();
    const [client, server] = Object.values(pair);
    console.log(`[room] socket accepted (queryToken=${queryToken ? "yes" : "no"})`);

    // Remember the query token + role on the socket itself: with the
    // hibernation API the DO class is re-instantiated per wake, so instance
    // fields are NOT a safe place for peer state. Attachments and storage are.
    server.serializeAttachment({ role: null, queryToken });

    // acceptWebSocket() both accepts the server side and registers it for
    // hibernation-driven webSocketMessage/webSocketClose callbacks.
    this.state.acceptWebSocket(server);
    return new Response(null, { status: 101, webSocket: client });
  }

  async webSocketMessage(ws, message) {
    let msg;
    try {
      msg = JSON.parse(message);
    } catch {
      return; // not JSON: ignore
    }

    const att = ws.deserializeAttachment() || {};
    if (msg.type !== "diag") {
      console.log(`[room] msg type=${msg.type} role=${att.role || "unassigned"}`);
    }
    if (msg.type === "ping") {
      return; // keepalive, never relayed
    }

    // First message must be hello; assigns the socket a role.
    if (!att.role) {
      if (msg.type !== "hello") return;
      const tokenOk =
        msg.token === this.env.TOKEN || att.queryToken === this.env.TOKEN;
      if (!tokenOk) {
        this.reject(ws, "bad token");
        return;
      }
      const role = msg.role;
      if (role !== "host" && role !== "client") {
        this.reject(ws, "bad role");
        return;
      }

      // Exactly one host + one client: kick any stale socket of the same role.
      const existing = this.findPeer(role);
      console.log(`[room] hello role=${role} existing=${existing ? "yes" : "none"}`);
      if (existing && existing !== ws) {
        this.reject(existing, `${role} replaced`);
      }
      // A fresh client means any buffered offer/ICE came from a previous,
      // now-dead session — delivering them to a future host would poison it
      // (the host accepts a stale offer, its DTLS times out, session dies).
      if (role === "client") {
        await this.state.storage.delete("pendingToHost");
      }
      ws.serializeAttachment({ ...att, role });
      ws.send(JSON.stringify({ type: "hello-ok" }));
      console.log(`[room] ${role} registered, hello-ok sent`);

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
    const target = att.role === "host" ? this.findPeer("client") : this.findPeer("host");
    if (msg.type !== "diag") {
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
      // Buffer offer/ice from the client until the host connects.
      if (att.role === "client" && (msg.type === "offer" || msg.type === "ice")) {
        const pending = (await this.state.storage.get("pendingToHost")) || [];
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
    console.log(`[room] socket closed role=${role || "unassigned"} replaced=${!!att.replaced}`);
    if (!role) return;
    // When the client dies, its buffered offer/ICE is worthless — clear it so
    // a later host never receives a stale offer from a dead peer.
    if (role === "client") {
      await this.state.storage.delete("pendingToHost");
    }
    if (att.replaced) return; // replaced sockets get no farewell
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
    try {
      console.log(`[room] REJECT: ${message}`);
      ws.send(JSON.stringify({ type: "error", message }));
      const att = ws.deserializeAttachment() || {};
      if (att.role) ws.serializeAttachment({ ...att, replaced: true });
      ws.close(1008, message);
    } catch {
      // socket already closed
    }
  }

  findPeer(role) {
    const sockets = this.state.getWebSockets();
    return (
      sockets.find((s) => {
        const a = s.deserializeAttachment() || {};
        return a.role === role && !a.replaced && s.readyState === 1;
      }) || null
    );
  }
}

function utf8Length(value) {
  return new TextEncoder().encode(value).byteLength;
}
