// Monitor selection is a signaling-level control with epoch-scoped
// confirmation. These helpers keep the ordering and invalidation rules
// testable without a browser.
//
// The host accepts exactly one offer per signaling epoch
// (host_session.rs should_forward_offer). A WebRTC monitor/source transition
// therefore has to start a new signaling epoch before restarting the peer:
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
