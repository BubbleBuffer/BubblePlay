// Signaling policy failures are page-local and must not retry into a newer
// client of the same one-client room. Network failures remain recoverable.
export function shouldReconnectAfterClose(closeCode, terminalFailure) {
  return !terminalFailure && closeCode !== 1008;
}

// The relay answers every signaling ping at once. A ping left unanswered this
// long means the socket is dead even if the browser still calls it open (a
// half-open TCP path after a network change), so the viewer recycles it. The
// clock starts at the oldest unanswered ping, so hidden-tab timer throttling,
// which only spaces the pings out, never trips it.
export const SIGNALING_ANSWER_LIMIT_MS = 30_000;
export function signalingUnanswered({ pingSentMs, inboundMs, nowMs, limitMs = SIGNALING_ANSWER_LIMIT_MS }) {
  return pingSentMs !== null && inboundMs < pingSentMs && nowMs - pingSentMs >= limitMs;
}

// A failed WebRTC transport can leave the host's old RTC epoch alive long
// enough to consume and ignore a replacement offer. Recycling signaling makes
// the relay notify the host first, so both sides converge on a fresh epoch.
export function shouldRecycleSignalingForPeerState(connectionState) {
  return connectionState === "failed";
}

// Once WSS carries media and input, the RTC peer is only a lifecycle anchor.
// Its failure or a closed data channel must end that peer, never the session.
export function rtcPeerEndAction({ usingWssMedia, connectionState = null, laneClosed = false } = {}) {
  if (!laneClosed && !shouldRecycleSignalingForPeerState(connectionState)) return "none";
  return usingWssMedia ? "end-peer" : "recycle";
}

// NetworkInformation.change is a broad estimate-change signal: Chrome may fire
// it when only RTT, downlink, or effectiveType changes. Treat it as a probe
// trigger, not proof that the active route is dead.
export function shouldRecoverAfterNetworkRouteProbe(before = {}, after = {}, {
  maxRenderedAgeMs = 1_500,
} = {}) {
  if (!after.online) return false;
  if (after.signalingGeneration !== before.signalingGeneration) return false;

  const progressed = Number(after.receivedPackets) > Number(before.receivedPackets) ||
    Number(after.reconstructed) > Number(before.reconstructed) ||
    Number(after.presented) > Number(before.presented);
  if (progressed) return false;
  if (!after.signalingRegistered || !after.signalingSocketOpen) return true;

  if (after.usingWssMedia) {
    if (after.mediaSocketOpen !== true || after.recoveryState !== "healthy") return true;
    if (after.allowQuiescentIdle === true) return false;
    return Number.isFinite(after.renderedAgeMs) && after.renderedAgeMs > maxRenderedAgeMs;
  }

  return after.peerConnectionState === "failed" || after.peerConnectionState === "disconnected";
}
