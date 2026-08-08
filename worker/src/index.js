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
//   host   -> worker : {type:"answer", sdp} | {type:"ice", candidate}
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
    return env.ASSETS.fetch(request);
  },
};

function handleWebSocket(request, env) {
  const url = new URL(request.url);
  const queryToken = url.searchParams.get("token") || "";
  const roomId = env.ROOM.idFromName("room");
  const stub = env.ROOM.get(roomId);
  return stub.fetch(request, {
    headers: {
      ...Object.fromEntries(request.headers),
      "x-bp-token": queryToken,
    },
  });
}

export class Room {
  constructor(state, env) {
    this.state = state;
    this.env = env;
    this.host = null;
    this.client = null;
    // Messages from the client that arrived before the host connected
    // (the browser creates its offer immediately after hello-ok).
    this.pendingToHost = [];
  }

  async fetch(request) {
    const queryToken = request.headers.get("x-bp-token") || "";
    const pair = new WebSocketPair();
    const [client, server] = Object.values(pair);

    // Remember the query token so webSocketMessage can accept it as an
    // alternative to the hello body token.
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
      if (role === "host") {
        if (this.host && this.host !== ws) {
          this.reject(this.host, "host replaced");
        }
        this.host = ws;
      } else {
        if (this.client && this.client !== ws) {
          this.reject(this.client, "client replaced");
        }
        this.client = ws;
      }
      ws.serializeAttachment({ ...att, role });
      ws.send(JSON.stringify({ type: "hello-ok" }));

      // Flush anything the client sent before the host arrived.
      if (role === "host" && this.pendingToHost.length) {
        for (const m of this.pendingToHost) {
          this.host.send(m);
        }
        this.pendingToHost = [];
      }
      return;
    }

    // Relay to the peer.
    const target = att.role === "host" ? this.client : this.host;
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
        this.pendingToHost.push(message);
        if (this.pendingToHost.length > 64) this.pendingToHost.shift();
      }
      return;
    }
    target.send(message);
  }

  async webSocketClose(ws, _code, _reason, _wasClean) {
    const att = ws.deserializeAttachment() || {};
    const role = att.role;
    if (!role) return;
    if (role === "host" && this.host === ws) this.host = null;
    if (role === "client" && this.client === ws) this.client = null;
    const peer = role === "host" ? this.client : this.host;
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
      ws.send(JSON.stringify({ type: "error", message }));
      ws.close(1008, message);
    } catch {
      // socket already closed
    }
  }
}
