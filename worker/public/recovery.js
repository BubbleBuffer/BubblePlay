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
