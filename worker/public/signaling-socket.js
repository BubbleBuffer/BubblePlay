// The signaling socket (/ws): connect, relay and encrypted-control sends, the reconnect backoff, the
// connect timeout, the wake retry and the recycle. It owns the socket and its timers through the
// shared session state; what arrives on it and what a close means for the peer are handed to the
// receiver through callbacks.
import { jitteredBackoffMsV4, nextBackoffMsV4 } from "./media-v4.js";
import { CLOSE_POLICY, shouldReconnectAfterClose, shouldRetryOnWake } from "./recovery.js";

const SOCKET_CONNECTING = 0;
const SOCKET_OPEN = 1;
const CLOSE_NORMAL = 1000;
const FIRST_CONNECT_REASON = "startup";

// env: state (the session state), recovery, hudState, url(), hello(), handleSignal(socket,
// generation, ev), cleanupPeer(), restartPeer(reason, reconnect), markBad(), onPolicyClose({ admitted,
// reason }), onMessageFailed(error), invalidateMonitor(reason), recycleDiagnostic(reason),
// log(level, event, detail), trace(name, detail), recordError(kind, message, detail), now() and
// optional makeSocket(url), random, setTimeout, clearTimeout (for tests).
export function createSignalingSocket(env) {
  const state = env.state;
  const makeSocket = env.makeSocket ?? (url => new WebSocket(url));
  const setT = env.setTimeout ?? setTimeout, clearT = env.clearTimeout ?? clearTimeout;

  // Relay-level messages only: hello, register-proof and the keepalive ping.
  function sendRelay(message) {
    if (state.ws?.readyState !== SOCKET_OPEN) return false;
    state.ws.send(JSON.stringify(message));
    return true;
  }

  // Every endpoint JSON message on /ws is encrypted; relay messages use sendRelay instead.
  function sendControl(message) {
    if (!state.signalingRegistered || state.ws?.readyState !== SOCKET_OPEN) return false;
    return state.authenticatedSession?.sendControl(message) || false;
  }

  function dropSession() {
    state.signalingRegistered = false;
    state.signalingPc = null;
    state.signalingTicket = null;
    state.authenticatedSession?.close();
    state.authenticatedSession = null;
  }

  function reconnect(reason) {
    if (state.terminalSignalingFailure) return;
    if (state.reconnectTimer || state.ws?.readyState === SOCKET_OPEN || state.ws?.readyState === SOCKET_CONNECTING) return;
    env.hudState.ws = `reconnecting (${reason})`;
    const connect = () => connectNow(reason);
    if (reason === FIRST_CONNECT_REASON) { connect(); return; }
    state.signalingBackoffMs = nextBackoffMsV4(state.signalingBackoffMs);
    state.pendingConnect = connect;
    state.reconnectTimer = setT(connect, jitteredBackoffMsV4(state.signalingBackoffMs, env.random));
  }

  function connectNow(reason) {
    state.reconnectTimer = null;
    state.pendingConnect = null;
    state.latestInputStatus = null;
    state.inputStatusReason = null;
    state.authenticatedSession?.close();
    state.authenticatedSession = null;
    const generation = ++state.signalingGeneration;
    env.recovery.noteConnectionOpened();
    const socketUrl = env.url();
    env.trace("signaling-construct", { path: new URL(socketUrl).pathname, generation, reason });
    const socket = makeSocket(socketUrl);
    socket.binaryType = "arraybuffer";
    state.ws = socket;
    armConnectTimeout(socket, generation);
    const current = () => state.ws === socket && generation === state.signalingGeneration;
    let receiveQueue = Promise.resolve();
    socket.onopen = () => {
      if (!current()) return;
      clearT(state.connectTimer);
      env.hudState.ws = "open";
      env.trace("signaling-open", { generation });
      sendRelay(env.hello());
    };
    const handleClose = event => {
      if (!current()) return;
      clearT(state.connectTimer);
      const admitted = state.signalingRegistered;
      env.recovery.noteLoss();
      env.hudState.ws = "closed";
      state.latestInputStatus = null;
      state.inputStatusReason = null;
      dropSession();
      env.cleanupPeer();
      const closeReason = event.reason || "";
      env.recordError("socket-close", `WebSocket closed (${event.code})`, { code: event.code, reason: closeReason });
      env.trace("signaling-close", { generation, code: event.code, reason: closeReason });
      env.log(event.code === CLOSE_POLICY ? "warn" : "info", "signaling-close", { generation, closeCode: event.code, reason: closeReason, admitted });
      env.markBad();
      if (event.code === CLOSE_POLICY) {
        // Policy close: refused before hello-ok, or dropped from the PC's list. Never reconnect.
        state.terminalSignalingFailure = true;
        env.onPolicyClose({ admitted, reason: event.reason });
        return;
      }
      if (!shouldReconnectAfterClose(event.code, state.terminalSignalingFailure)) {
        state.terminalSignalingFailure = true;
        return;
      }
      env.restartPeer("ws-closed", false);
      reconnect("ws-closed");
    };
    socket.onerror = () => {
      if (state.ws !== socket) return;
      env.hudState.ws = "error";
      env.recordError("socket-error", "WebSocket signaling error");
      env.trace("signaling-error", { generation });
    };
    // WebCrypto yields between messages. Preserve wire order across hello-ok,
    // auth-host and ctl rather than letting WebSocket callbacks race each other.
    socket.onmessage = ev => {
      if (state.ws === socket) state.signalingInboundMs = env.now();
      receiveQueue = receiveQueue.then(() => env.handleSignal(socket, generation, ev)).catch(error => {
        if (current()) env.onMessageFailed(error);
      });
    };
    socket.onclose = event => {
      // A final encrypted refusal precedes close on the wire, but decryption
      // may still be running. Let that refusal decide whether to reconnect.
      receiveQueue = receiveQueue.then(() => handleClose(event));
    };
  }

  // A reconnect of an established session abandons a socket that has not opened in time (5, 7.5, then
  // 10 s) and tries a new one. Handlers are detached first so a late event of the old socket is ignored.
  function armConnectTimeout(socket, generation) {
    clearT(state.connectTimer);
    const waitedMs = env.recovery.connectTimeoutMs();
    if (waitedMs === null) return;
    state.connectTimer = setT(() => {
      if (state.ws !== socket || generation !== state.signalingGeneration || socket.readyState !== SOCKET_CONNECTING) return;
      env.log("info", "signaling-connect-abandoned", { connectAttempt: env.recovery.attempts, waitedMs });
      socket.onopen = socket.onmessage = socket.onerror = socket.onclose = null;
      state.ws = null;
      state.signalingGeneration++;
      try { socket.close(); } catch (error) { env.log("info", "signaling-abandon-close-failed", { error: String(error?.message || error) }); }
      reconnect("connect-timeout");
    }, waitedMs);
  }

  // The page was frozen or came back online: a pending reconnect retries now instead of waiting for a timer.
  function wake(event) {
    if (!shouldRetryOnWake({ timerPending: state.reconnectTimer !== null && state.pendingConnect !== null,
      terminal: state.terminalSignalingFailure, connecting: state.ws?.readyState === SOCKET_CONNECTING })) return;
    const connect = state.pendingConnect;
    clearT(state.reconnectTimer); state.reconnectTimer = null; state.pendingConnect = null;
    env.log("info", "reconnect-wake", { event });
    connect();
  }

  function recycle(reason) {
    if (state.terminalSignalingFailure) return;
    env.recovery.noteLoss();
    state.latestInputStatus = null;
    state.inputStatusReason = null;
    env.invalidateMonitor(reason);
    const diagnostic = env.recycleDiagnostic(reason);
    state.lastSignalingRecycle = { ...diagnostic, atMs: Date.now() };
    env.trace("signaling-recycle", diagnostic);
    env.log("warn", "signaling-recycle", diagnostic);
    // Invalidate first so the intentional close cannot restart a peer with
    // credentials from the old signaling epoch.
    const socket = state.ws;
    state.ws = null;
    dropSession();
    state.signalingGeneration++;
    try { socket?.close(CLOSE_NORMAL, reason); } catch (error) { env.log("info", "signaling-recycle-close-failed", { error: String(error?.message || error) }); }
    reconnect(reason);
  }

  return { sendRelay, sendControl, reconnect, recycle, wake };
}
