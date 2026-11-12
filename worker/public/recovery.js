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

// The WSS v4 media socket gets the same rule. The viewer pings it every heartbeat whatever else it
// sends (its acks used to postpone the ping, leaving a static desktop with no inbound traffic but
// the host's rare frames: healthy sockets were closed as silent), and the relay answers each ping.
// The socket is dead only when a ping stays unanswered this long, counted from the oldest
// unanswered ping. Hidden-tab timer throttling only spaces the pings out, so it never trips it.
// 25 s after a ping that follows 15 s of quiet is the old 40 s of silence.
export const MEDIA_ANSWER_LIMIT_MS = 25_000;
/** One heartbeat tick on `state` ({ pingSentMs, lastPingAt, lastInboundAt }): "ping", "dead" or "none". */
export function mediaHeartbeatStep(state, nowMs, { heartbeatMs, limitMs = MEDIA_ANSWER_LIMIT_MS }) {
  if (signalingUnanswered({ pingSentMs: state.pingSentMs, inboundMs: state.lastInboundAt, nowMs, limitMs })) return "dead";
  if (nowMs - state.lastPingAt < heartbeatMs) return "none";
  state.lastPingAt = nowMs;
  if (state.pingSentMs === null || state.lastInboundAt >= state.pingSentMs) state.pingSentMs = nowMs;
  return "ping";
}

// Time the page was frozen or the PC asleep must not count as recovery time ("recovered after
// 856700 ms" after a lid close, when the real recovery took 3.9 s). The tracker adds up suspended
// time from two sources: Page Lifecycle freeze/resume, and a gap between two ticks of the 15 s
// heartbeat longer than a throttled hidden tab can explain (its timers still fire about once a
// minute). Times are performance.now() milliseconds.
export const SUSPEND_GAP_MS = 90_000;
export function createSuspendTracker({ gapMs = SUSPEND_GAP_MS } = {}) {
  let lastTick = null, frozenAt = null, total = 0;
  return {
    /** Call on every heartbeat tick. */
    tick(nowMs) {
      if (frozenAt === null && lastTick !== null && nowMs - lastTick > gapMs) total += nowMs - lastTick;
      lastTick = nowMs;
    },
    freeze(nowMs) { if (frozenAt === null) frozenAt = nowMs; },
    resume(nowMs) {
      if (frozenAt === null) return;
      total += nowMs - frozenAt;
      frozenAt = null;
      lastTick = nowMs; // the gap that follows is already counted
    },
    /** Suspended milliseconds so far (an open freeze counts up to `nowMs`). */
    totalMs(nowMs) { return total + (frozenAt === null ? 0 : nowMs - frozenAt); },
  };
}
/** Split a recovery into wall-clock, suspended and active milliseconds. */
export function splitRecovery(wallMs, suspendedMs) {
  const suspended = Math.min(Math.max(Math.round(suspendedMs), 0), Math.round(wallMs));
  return { recoveryMs: Math.round(wallMs), suspendedMs: suspended, activeMs: Math.round(wallMs) - suspended };
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

// A relay reset drops every socket. The viewer is admitted again within about a
// second, but the PC may re-register a few seconds later and so not answer the
// first auth-hello. For a session that was already established, a silent PC is
// retried on a fresh signaling connection instead of ending the session.
export const RECYCLED_AUTH_TIMEOUT_MS = 3_000;
export const RECYCLED_AUTH_TIMEOUT_MAX_MS = 8_000;
export const RECOVERY_BUDGET_MS = 30_000;

/** Wait for auth-host on attempt `attempt` (0 = the first since the loss): 3 s, 4.5 s, 6.75 s, then 8 s. */
export function recycledAuthTimeoutMs(attempt) {
  const step = Number.isInteger(attempt) && attempt > 0 ? attempt : 0;
  return Math.min(RECYCLED_AUTH_TIMEOUT_MS * 1.5 ** step, RECYCLED_AUTH_TIMEOUT_MAX_MS);
}

/**
 * True when a failed session should be retried rather than end on the terminal screen.
 * Only the silent-PC timeout of a session that was established before is retried, and only
 * while the recovery budget lasts. Refusals (bad key, not paired, 1008, missing cap) never are.
 */
export function shouldRetryAuthTimeout({ reason, established, budgetStartMs, nowMs, budgetMs = RECOVERY_BUDGET_MS }) {
  return reason === "auth-timeout" && established === true && Number.isFinite(budgetStartMs) &&
    nowMs - budgetStartMs < budgetMs;
}

// A reconnect of an established session can sit on a browser socket that never opens (an 11 s
// construct-to-open was seen after a relay reset). Such a socket is abandoned and replaced.
// A first connection is not timed: it may wait on the site gate.
export const CONNECT_TIMEOUT_MS = 5_000;
export const CONNECT_TIMEOUT_MAX_MS = 10_000;

/** Wait for a signaling socket to open on attempt `attempt` (0 = the first since the loss): 5 s, 7.5 s, then 10 s. Null when not recovering. */
export function connectTimeoutMs({ recovering, attempt }) {
  if (recovering !== true) return null;
  const step = Number.isInteger(attempt) && attempt > 0 ? attempt : 0;
  return Math.min(CONNECT_TIMEOUT_MS * 1.5 ** step, CONNECT_TIMEOUT_MAX_MS);
}

/**
 * On wake (page visible again, pageshow, online) a pending reconnect retries at once, as its timer
 * may have been lost while the page was frozen. Not when a socket is already connecting or the
 * session ended.
 */
export function shouldRetryOnWake({ timerPending, terminal, connecting }) {
  return timerPending === true && terminal !== true && connecting !== true;
}
