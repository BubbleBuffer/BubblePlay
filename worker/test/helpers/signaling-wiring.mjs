// Builds the real signaling socket (public/signaling-socket.js) inside a vm harness that runs slices
// of receiver.js. The slices call sendRelay, sendControl, reconnectSignaling, recycleSignaling and
// wakeReconnect as globals; this defines them from the module, wired to the callbacks the harness
// sandbox provides, the way receiver.js wires them to its own functions.
import { createSignalingSocket } from "../../public/signaling-socket.js";

export function attachSignaling(context, overrides = {}) {
  const signaling = createSignalingSocket({
    state: context.sessionState,
    get recovery() { return context.recovery; },
    get hudState() { return context.hudState; },
    url: () => context.signalingSocketUrl(),
    hello: () => context.viewerHello({ identity: context.sessionIdentity, turn: false }),
    handleSignal: (socket, generation, ev) => context.handleSignal(socket, generation, ev),
    cleanupPeer: () => context.cleanupPeer?.(),
    restartPeer: (reason, reconnect) => context.restartPeer?.(reason, reconnect),
    markBad: () => context.hud?.classList.add("bad"),
    onPolicyClose: ({ admitted, reason }) => { context.releaseInput?.(); if (!admitted) context.showNotAdmitted?.(reason); },
    onMessageFailed: () => context.endAuthenticatedSession?.({ title: "Session ended", body: "A control message failed its integrity check, so the session was closed." }),
    invalidateMonitor: reason => context.invalidatePendingMonitorRequest?.(reason),
    recycleDiagnostic: reason => ({ reason, ...(context.recycleDiagnosticFields ?? {}) }),
    log: (level, event, detail) => context.viewerLog?.(level, event, detail),
    trace: (stage, detail) => context.debugTrace?.(stage, detail),
    recordError: (kind, message, detail) => context.recordSignalingError?.(kind, message, detail),
    now: () => context.performance?.now() ?? 0,
    makeSocket: url => new context.WebSocket(url),
    random: () => 0.5,
    setTimeout: (fn, delay) => context.setTimeout(fn, delay),
    clearTimeout: id => context.clearTimeout(id),
    ...overrides,
  });
  context.sendRelay = signaling.sendRelay;
  context.sendControl = signaling.sendControl;
  context.reconnectSignaling = signaling.reconnect;
  context.recycleSignaling = signaling.recycle;
  context.wakeReconnect = signaling.wake;
  return signaling;
}
