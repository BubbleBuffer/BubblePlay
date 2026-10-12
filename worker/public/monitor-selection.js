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
// frame on screen. Monitor and desktop-source changes (secure desktop,
// Winlogon, session change) both stay on the existing peer when the browser
// can prove which frame belongs to the new host media generation: WSS frames
// carry the generation; on WebRTC the host's first-frame admission carries the
// RTP timestamp of the new generation's IDR (rtcAdmissionGate below). Without
// that proof (`rtcInPlace` false) a WebRTC transition recreates the peer, so
// an old frame can never authorize input on another desktop or monitor.
// Monitor switches carry held input; desktop-source changes release it.
// Returns null when the message is not such a transition.
export const DESKTOP_SOURCE_REASONS = Object.freeze(["desktop-switched", "capture-recovered", "session-changed", "agent-restarted"]);
export function desktopTransitionPlan({ reason, state, usingWss, alreadyRendering, rtcInPlace = false }) {
  if (!alreadyRendering) return null;
  const inPlace = Boolean(usingWss || rtcInPlace);
  if (reason === "monitor-switch" || reason === "monitor-switched") {
    if (state !== "active") return { kind: "monitor", holdInput: true, action: "wait" };
    return inPlace ? { kind: "monitor", holdInput: true, action: "handshake" } : { kind: "monitor", holdInput: false, action: "restart-peer" };
  }
  if (state === "active" && DESKTOP_SOURCE_REASONS.includes(reason))
    return { kind: "source", holdInput: false, action: inPlace ? "handshake" : "restart-peer" };
  return null;
}

// RTP timestamps are 32-bit and wrap about every 13 hours at 90 kHz. Compare
// them with serial-number arithmetic (RFC 1982): `frame` is at or after
// `admitted` when the forward distance is under half the number space.
export function isRtpTimestamp(value) {
  return Number.isInteger(value) && value >= 0 && value <= 0xffff_ffff;
}
export function rtpTimestampAtOrAfter(frame, admitted) {
  if (!isRtpTimestamp(frame) || !isRtpTimestamp(admitted)) return false;
  return ((frame - admitted) >>> 0) < 0x8000_0000;
}

// Whether this browser can prove a WebRTC frame's generation, so a transition
// can stay in place. path: "native" (<video>), "worker"
// (RTCRtpScriptTransform + WebCodecs) or "decoder" (createEncodedStreams +
// WebCodecs). Native video needs requestVideoFrameCallback reporting
// metadata.rtpTimestamp; the WebCodecs paths gate on a keyframe after
// admission themselves.
export function rtcInPlaceCapable({ path, rvfc = false, rtpTimestamps = false }) {
  if (path === "native") return Boolean(rvfc && rtpTimestamps);
  return path === "worker" || path === "decoder";
}

// Decide, on the first `admitted` of a WebRTC handshake, how its presentation
// is proven. Returns { action: "gate", gate } to wait for a qualifying frame,
// { action: "restart-peer" } when an in-place transition cannot be proven
// (old host without rtpTimestamp, or a browser without rVFC rtpTimestamp), or
// { action: "legacy" } for other handshakes, which keep today's behaviour.
export function rtcAdmissionGate({ transition, path, admittedRtpTimestamp, rvfc = false, rtpTimestamps = false }) {
  const hostProof = isRtpTimestamp(admittedRtpTimestamp);
  if (transition && !hostProof) return { action: "restart-peer", reason: "no-rtp-timestamp" };
  if (path === "native") {
    if (hostProof && rvfc && rtpTimestamps) return { action: "gate", gate: { mode: "native", admittedRtpTimestamp } };
    return transition ? { action: "restart-peer", reason: "no-rvfc-rtp-timestamp" } : { action: "legacy" };
  }
  if (path === "worker" || path === "decoder")
    return { action: "gate", gate: { mode: path, admittedRtpTimestamp: hostProof ? admittedRtpTimestamp : null, strict: Boolean(transition) } };
  return transition ? { action: "restart-peer", reason: "unknown-media-path" } : { action: "legacy" };
}

// Whether a presentation opens the input gate. Native video needs a presented
// frame whose rtpTimestamp is at or after the admitted IDR's. The WebCodecs
// paths drop every frame until transitionKeyframeAdmits() passes, so they
// report `gatePassed` once a frame decoded after that point is drawn.
export function rtcGateOpens(gate, proof = null) {
  if (!gate) return true;
  if (gate.mode === "native") return rtpTimestampAtOrAfter(proof?.rtpTimestamp, gate.admittedRtpTimestamp);
  return proof?.gatePassed === true;
}

// The WebCodecs decode-side filter after admission: "admit" a keyframe that is
// at or after the admitted IDR when both timestamps are known, "drop" anything
// else. A keyframe without an RTP timestamp is "unprovable" for a transition
// (strict), which falls back to a peer restart; for other handshakes
// keyframe-after-admission stays the proof.
export function transitionKeyframeVerdict({ isKey, frameRtpTimestamp, admittedRtpTimestamp, strict = false }) {
  if (!isKey) return "drop";
  if (!isRtpTimestamp(admittedRtpTimestamp)) return strict ? "unprovable" : "admit";
  if (!isRtpTimestamp(frameRtpTimestamp)) return strict ? "unprovable" : "admit";
  return rtpTimestampAtOrAfter(frameRtpTimestamp, admittedRtpTimestamp) ? "admit" : "drop";
}
export function transitionKeyframeAdmits(frame) {
  return transitionKeyframeVerdict(frame) === "admit";
}
