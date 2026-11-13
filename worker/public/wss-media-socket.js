// The WSS media socket (/media-ws): connect with a ticket, hello-ok, heartbeat, reconnect on the
// media backoff, and the connect timeout. It owns the socket and its timers; what arrives on it is
// handed to the receiver through callbacks. Binary packets and host presence are not interpreted here.
import { V4_HEARTBEAT_MS, jitteredBackoffMsV4, nextBackoffMsV4 } from "./media-v4.js";
import { MEDIA_CONNECT_TIMEOUT_MS, mediaHeartbeatStep } from "./recovery.js";

export const MEDIA_HEARTBEAT_TICK_MS = 1_000;
export const MEDIA_HELLO = Object.freeze({ type: "hello", role: "media-viewer" });
const CLOSE_NORMAL = 1000;
const CLOSE_SILENT = 4000;
const SOCKET_OPEN = 1;

// env: active() (media runs over WSS), epoch() (truthy when a media epoch is set), ticket() (the
// current media ticket, null unless the session is media-ready), url(), now(), onPacket(data),
// onOpen({ reconnect }), onPeer({ present, joined }), onClosed(), log(level, event, detail),
// trace(name, detail), makeSocket(url) and timers (optional, for tests).
export function createWssMediaSocket(env) {
  const makeSocket = env.makeSocket ?? (url => new WebSocket(url));
  const setT = env.setTimeout ?? setTimeout, clearT = env.clearTimeout ?? clearTimeout;
  const setI = env.setInterval ?? setInterval, clearI = env.clearInterval ?? clearInterval;
  // Heartbeat bookkeeping is read and updated by recovery.js mediaHeartbeatStep.
  const beat = { lastInboundAt: 0, lastPingAt: 0, pingSentMs: null };
  let socket = null, socketOpen = false, everOpened = false, hostPresent = false, reconnecting = false;
  let backoffMs = 0, reconnects = 0;
  let heartbeatTimer = null, connectTimer = null, reconnectTimer = null;

  function send(text) {
    if (!socket || socket.readyState !== SOCKET_OPEN) return false;
    try { socket.send(text); } catch { return false; }
    return true;
  }

  function scheduleReconnect(reason) {
    clearT(reconnectTimer);
    backoffMs = nextBackoffMsV4(backoffMs);
    reconnecting = true;
    env.trace("media-v4-reconnect-scheduled", { reason, delayMs: backoffMs });
    reconnectTimer = setT(() => {
      reconnectTimer = null;
      if (!env.active()) return;
      reconnects++;
      open();
    }, jitteredBackoffMsV4(backoffMs, env.random));
  }

  function detach(target) { target.onopen = target.onmessage = target.onerror = target.onclose = null; }

  function open() {
    if (!env.active() || !env.epoch() || !env.ticket()) return;
    const ticket = env.ticket();
    const target = makeSocket(env.url());
    target.binaryType = "arraybuffer";
    socket = target; socketOpen = false; hostPresent = false;
    const current = () => env.active() && socket === target && env.ticket() === ticket;
    clearT(connectTimer);
    connectTimer = setT(() => {
      connectTimer = null;
      if (!current() || socketOpen) return;
      env.log("warn", "wss-v4-connect-timeout", { waitedMs: MEDIA_CONNECT_TIMEOUT_MS, reconnects });
      detach(target);
      socket = null;
      try { target.close(); } catch { /* already closing or closed */ }
      scheduleReconnect("connect-timeout");
    }, MEDIA_CONNECT_TIMEOUT_MS);
    target.onopen = () => {
      if (!current()) return;
      target.send(JSON.stringify({ ...MEDIA_HELLO, ticket }));
      beat.lastInboundAt = beat.lastPingAt = env.now();
      beat.pingSentMs = null;
    };
    target.onmessage = event => {
      if (!current()) return;
      beat.lastInboundAt = env.now();
      if (typeof event.data !== "string") { env.onPacket(event.data); return; }
      let message;
      try { message = JSON.parse(event.data); } catch { return; } // not JSON: ignored like any unknown text
      if (message.type === "hello-ok") {
        clearT(connectTimer); connectTimer = null;
        const reconnect = everOpened;
        socketOpen = everOpened = true;
        reconnecting = false;
        backoffMs = 0;
        env.trace("media-v4-open", { reconnect, reconnects });
        if (reconnect) env.log("info", "wss-v4-reconnected", { reconnects });
        env.onOpen({ reconnect });
      } else if (message.type === "media-peer") {
        const present = message.present === true;
        const joined = present && !hostPresent;
        hostPresent = present;
        env.onPeer({ present, joined });
      }
    };
    target.onclose = event => {
      if (!current()) return;
      clearT(connectTimer); connectTimer = null;
      socketOpen = hostPresent = false;
      socket = null;
      env.trace("media-v4-close", { code: event.code, reason: event.reason || "", wasClean: event.wasClean });
      env.onClosed();
      scheduleReconnect("socket-close");
    };
    target.onerror = () => {
      if (!current()) return;
      try { target.close(); } catch { /* already closed */ }
    };
  }

  return {
    send, open,
    // Starts the heartbeat and the first connection.
    start() {
      clearI(heartbeatTimer);
      heartbeatTimer = setI(() => {
        if (!env.active() || !socketOpen) return;
        const now = env.now();
        // The relay answers every heartbeat; a ping left unanswered means a half-open socket.
        // Acks and other traffic never postpone the ping.
        const step = mediaHeartbeatStep(beat, now, { heartbeatMs: V4_HEARTBEAT_MS });
        if (step === "ping") send('{"type":"ping"}');
        else if (step === "dead") {
          env.log("warn", "wss-v4-silent-socket", { silentMs: Math.round(now - beat.lastInboundAt), unansweredMs: Math.round(now - beat.pingSentMs) });
          try { socket?.close(CLOSE_SILENT, "silent"); } catch { /* already closed */ }
        }
      }, MEDIA_HEARTBEAT_TICK_MS);
      open();
    },
    // Timers only: the socket stays (a restart reuses it).
    clearTimers() {
      clearI(heartbeatTimer); heartbeatTimer = null;
      clearT(reconnectTimer); reconnectTimer = null;
      clearT(connectTimer); connectTimer = null;
    },
    // Close and forget everything about this media session.
    stop() {
      this.clearTimers();
      try { socket?.close(CLOSE_NORMAL, "switch"); } catch { /* already closed */ }
      socket = null; socketOpen = everOpened = hostPresent = reconnecting = false;
      backoffMs = 0;
    },
    get isOpen() { return socketOpen; },
    get hostPresent() { return hostPresent; },
    get reconnecting() { return reconnecting; },
    get reconnects() { return reconnects; },
  };
}
