// Monitor selection is a signaling-level control with epoch-scoped
// confirmation. These helpers keep the ordering and invalidation rules
// testable without a browser.
//
// The host accepts exactly one offer per signaling epoch
// (host_session.rs should_forward_offer). A WebRTC desktop-source transition,
// or a monitor transition whose in-place handshake failed, therefore has to start a new signaling epoch before restarting the peer:
// an offer sent into the old epoch is dropped, and the closed peer instead
// ends the whole host session, which drops the runtime-selected display.
export function restartPeerInNewEpoch({ recycleSignaling, restartPeer }, reason) {
  if (typeof recycleSignaling !== "function" || typeof restartPeer !== "function")
    throw new TypeError("restartPeerInNewEpoch requires recycleSignaling and restartPeer");
  recycleSignaling(reason);
  restartPeer(reason, false);
  return reason;
}

// The monitor control travels over the signaling socket. Input data channels
// are not a precondition: the host owns the switch, releases input, and
// requires a fresh admitted frame before media/input readiness returns.
export function monitorRequestReadiness({ socketOpen, signalingRegistered, pending }) {
  if (typeof pending === "string" && pending.length > 0) return { allowed: false, reason: "pending" };
  if (socketOpen !== true) return { allowed: false, reason: "signaling-socket" };
  if (signalingRegistered !== true) return { allowed: false, reason: "signaling-registration" };
  return { allowed: true, reason: null };
}

// A confirmation can only arrive for the epoch that issued the request. Any
// peer/signaling restart invalidates the outstanding request instead of
// latching monitor selection off for the rest of the page.
export function monitorRequestAfterRestart(pending, reason) {
  if (typeof pending !== "string" || pending.length === 0) return null;
  return { selector: pending, reason: String(reason || "epoch-restart"), invalidated: true };
}

// A desktop-state transition while a frame is already visible keeps that
// frame on screen. Monitor switches stay on the existing peer on both
// transports: the host bumps its media generation and the first-frame
// handshake (admission plus keyframe gating) proves the new monitor, while
// held input is carried. Desktop-source changes (secure desktop, Winlogon,
// session change) recreate the WebRTC peer, because native video exposes no
// host generation and an old frame must never authorize input on another
// desktop. Returns null when the message is not such a transition.
export const DESKTOP_SOURCE_REASONS = Object.freeze(["desktop-switched", "capture-recovered", "session-changed", "agent-restarted"]);
export function desktopTransitionPlan({ reason, state, usingWss, alreadyRendering }) {
  if (!alreadyRendering) return null;
  if (reason === "monitor-switch" || reason === "monitor-switched")
    return { kind: "monitor", holdInput: true, action: state === "active" ? "handshake" : "wait" };
  if (state === "active" && DESKTOP_SOURCE_REASONS.includes(reason))
    return { kind: "source", holdInput: false, action: usingWss ? "handshake" : "restart-peer" };
  return null;
}
