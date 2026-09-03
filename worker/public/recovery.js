// Signaling policy failures are page-local and must not retry into a newer
// client of the same one-client room. Network failures remain recoverable.
export function shouldReconnectAfterClose(closeCode, terminalFailure) {
  return !terminalFailure && closeCode !== 1008;
}

// A failed WebRTC transport can leave the host's old RTC epoch alive long
// enough to consume and ignore a replacement offer. Recycling signaling makes
// the relay notify the host first, so both sides converge on a fresh epoch.
export function shouldRecycleSignalingForPeerState(connectionState) {
  return connectionState === "failed";
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
    if (Number(after.openLanes) < Number(after.dataShards) || after.recoveryState !== "healthy") return true;
    if (after.allowQuiescentIdle === true) return false;
    return Number.isFinite(after.renderedAgeMs) && after.renderedAgeMs > maxRenderedAgeMs;
  }

  return after.peerConnectionState === "failed" || after.peerConnectionState === "disconnected";
}
