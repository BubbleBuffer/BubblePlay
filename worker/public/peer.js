// The WebRTC peer of one session: build it and send the offer (start), tear it down (cleanup) and
// start over (restart). It owns the offer-failure count; the peer, its data channels and the other
// shared fields are on the session state. What a track, a data-channel message or a connection-state
// change means for the page is handed to the receiver through callbacks.
import { createRtcStartupFallback } from "./media.js";
import { OFFER_FAILURE_LIMIT, rtcPeerEndAction } from "./recovery.js";
import { STUN_FALLBACK } from "./turn.js";
import { TRANSPORT, orderRtpCodecPreferences, rtpCodecMimeTypes } from "./transport.js";

const SOCKET_OPEN = 1;
const MEDIA_VIDEO = "video";
const MEDIA_AUDIO = "audio";
const DIRECTION_RECV_ONLY = "recvonly";
const INPUT_CONTROL_LABEL = "input-control";
const INPUT_MOTION_LABEL = "input-motion";
const BINARY_TYPE = "arraybuffer";
const CODEC_HEVC = "hevc";
const SET_CODEC_PREFERENCES_WARNING = "setCodecPreferences";
const MSG_ICE = "ice";
const MSG_OFFER = "offer";
const REASON_INPUT_LANE_CLOSED = "input-lane-closed";
const REASON_NO_RTP = "no-rtp";
const REASON_ICE_FAILED = "ice-failed";
const REASON_OFFER_FAILED = "offer-failed";
const REASON_CONSTRUCT_FAILED = "rtc-construct-failed";
const HANDSHAKE_PEER_CONNECTED = "ice-connected";
const HANDSHAKE_OFFER_SENT = "offer-sent";
const CONNECTION_CONNECTED = "connected";
const ICE_CONNECTED = [CONNECTION_CONNECTED, "completed"];
const CONNECTION_STARTING = ["new", "connecting"];
const ICE_STATE_FAILED = "failed";
// One receive transceiver each for video and audio.
const PENDING_RTC_TRACKS_MAX = 2;
const HUD_ICE_CONFIG_ERROR = "ice-config-error";

const socketOpen = state => state.ws?.readyState === SOCKET_OPEN;

function openPeerConnection(env, newPeerConnection, peerOptions) {
  const state = env.state;
  try {
    return newPeerConnection(peerOptions);
  } catch (firstError) {
    env.viewerLog("warn", "rtc-construct-failed", { error: String(firstError?.message || firstError), stunFallback: state.currentIceServers === STUN_FALLBACK });
    if (state.currentIceServers === STUN_FALLBACK) return null;
  }
  state.currentIceServers = STUN_FALLBACK;
  try {
    return newPeerConnection({ ...peerOptions, iceServers: STUN_FALLBACK });
  } catch (error) {
    env.viewerLog("error", "rtc-construct-failed", { error: String(error?.message || error), stunFallback: true });
    return null;
  }
}

// WebRTC cannot start in this browser. Auto mode moves to WSS at once; a forced WebRTC page has no
// other transport to use, so it shows the error.
function constructionFailed(env) {
  if (env.requestedTransport === TRANSPORT.AUTO) {
    env.switchToWss(REASON_CONSTRUCT_FAILED);
    return;
  }
  env.hudState.ice = HUD_ICE_CONFIG_ERROR;
  env.markBad();
}

function armStartupFallback(env, candidate, generation) {
  const state = env.state;
  state.rtcStartupFallback?.stop();
  state.rtcOfferSignaled = false;
  if (env.requestedTransport !== TRANSPORT.AUTO) return;
  state.rtcStartupFallback = createRtcStartupFallback({
    isCurrent: () => env.isCurrentPeer(generation, candidate) && !state.usingWssMedia && !state.wssSelected,
    snapshot: () => ({
      connected: ICE_CONNECTED.includes(candidate.iceConnectionState),
      peerConnected: candidate.connectionState === CONNECTION_CONNECTED,
      ...env.startupSnapshot(generation),
    }),
    getStats: () => candidate.getStats(),
    onFallback: () => env.switchToWss(REASON_NO_RTP),
    onHealthy: () => env.onRtcHealthy(),
  });
}

function addTransceivers(candidate) {
  // Receive-only video; the browser offers, the host answers (locked).
  const transceiver = candidate.addTransceiver(MEDIA_VIDEO, { direction: DIRECTION_RECV_ONLY });
  // PC sound: receive-only Opus after the video m-line. Audio is not gated on video.
  candidate.addTransceiver(MEDIA_AUDIO, { direction: DIRECTION_RECV_ONLY });
  transceiver.receiver.playoutDelayHint = 0; // minimal jitter buffer
  // playoutDelayHint is advisory and Chromium may still grow hundreds of
  // milliseconds of buffer. jitterBufferTarget is the explicit millisecond
  // target in current Chromium; zero chooses latency over concealment.
  if ("jitterBufferTarget" in transceiver.receiver) {
    try { transceiver.receiver.jitterBufferTarget = 0; } catch { /* older implementation */ }
  }
  return transceiver;
}

// Prefer HEVC for a fresh page, with H.264 after it so an H.264-only host can still answer. The
// preference and capability/decode fallback are page-local; no selection is persisted between
// sessions. Returns false when the page fell back to H.264 and the peer has to be built again.
function applyCodecPreferences(env, candidate, transceiver, receiverCapabilities) {
  try {
    const codecs = orderRtpCodecPreferences(receiverCapabilities()?.codecs, env.codecSession.codec);
    const primary = rtpCodecMimeTypes(env.codecSession.codec);
    const primaryAvailable = codecs.some(codec => primary.includes(codec.mimeType.toLowerCase()));
    if (codecs.length && primaryAvailable) {
      transceiver.setCodecPreferences(codecs);
    } else if (env.codecSession.codec === CODEC_HEVC && env.codecSession.fallbackToH264()) {
      env.state.offerSent = false;
      try { candidate.close(); } catch { /* already closed */ }
      return false;
    }
  } catch (error) {
    console.warn(SET_CODEC_PREFERENCES_WARNING, error);
  }
  return true;
}

// Reliable ordered state lane plus lossy latest-only motion lane. A lane that closes under a live
// peer recycles signaling and restarts the peer (restart(reason, reconnect)).
function attachInputLanes(env, candidate, generation, restart) {
  const state = env.state;
  const channel = candidate.createDataChannel(INPUT_CONTROL_LABEL, { ordered: true });
  const motionChannel = candidate.createDataChannel(INPUT_MOTION_LABEL, { ordered: false, maxRetransmits: 0 });
  state.dc = channel;
  state.motionDc = motionChannel;
  channel.binaryType = BINARY_TYPE;
  motionChannel.binaryType = BINARY_TYPE;
  const current = () => env.isCurrentPeer(generation, candidate);
  const laneClosed = () => {
    if (!current() || state.closingPeer) return;
    if (state.wssSelected || rtcPeerEndAction({ usingWssMedia: state.usingWssMedia, laneClosed: true }) === "end-peer") return env.endRtcPeerOnly(REASON_INPUT_LANE_CLOSED);
    env.releaseInput(false); env.recycleSignaling(REASON_INPUT_LANE_CLOSED); restart(REASON_INPUT_LANE_CLOSED, false);
  };
  const laneOpened = () => { if (current()) { state.rtcStartupFallback?.progress(); env.updateInputReady(); } };
  const cursorMessage = event => { if (current() && !state.usingWssMedia && state.authenticatedSession?.mediaReady) env.handleCursorState(event.data); };
  channel.onopen = laneOpened;
  channel.onclose = laneClosed;
  motionChannel.onopen = laneOpened;
  motionChannel.onclose = laneClosed;
  channel.onerror = event => { if (current()) env.hudState.dc = `err:${event.message || ""}`; };
  channel.onmessage = cursorMessage;
  motionChannel.onmessage = cursorMessage;
}

function attachIceHandlers(env, candidate, generation) {
  const state = env.state;
  const current = () => env.isCurrentPeer(generation, candidate);
  candidate.onicecandidate = ev => {
    if (current() && ev.candidate) env.sendControl({ type: MSG_ICE, candidate: ev.candidate.candidate });
  };
  candidate.onicecandidateerror = ev => {
    if (!current()) return;
    // Keep this compact and omit address/port so profiling never reports a
    // client's private or public IP address through the signaling relay.
    state.lastIceCandidateError = { errorCode: ev.errorCode, errorText: ev.errorText, url: ev.url, atMs: Date.now() };
  };
  candidate.oniceconnectionstatechange = () => {
    if (!current()) return;
    env.hudState.ice = candidate.iceConnectionState;
    state.rtcStartupFallback?.progress();
    state.connected = ICE_CONNECTED.includes(candidate.iceConnectionState);
    if (candidate.iceConnectionState === ICE_STATE_FAILED && env.requestedTransport === TRANSPORT.AUTO
        && !state.usingWssMedia && !state.wssSelected && !CONNECTION_STARTING.includes(candidate.connectionState)) {
      env.switchToWss(REASON_ICE_FAILED);
      return;
    }
    if (state.connected) {
      env.markGood();
      if (!env.mobilePointerDevice) env.requestPointerLock();
      // Usually idempotent with the offer-time request. If slow ICE outlasted
      // both watchdog intervals, renew the failed handshake once media can flow.
      env.startFirstFrameHandshake(HANDSHAKE_PEER_CONNECTED);
    }
  };
  candidate.ontrack = ev => {
    if (!current() || state.usingWssMedia || state.wssSelected) { ev.track.stop(); return; }
    if (state.authenticatedSession?.mediaReady) { env.onTrack(ev, generation); return; }
    ev.track.enabled = false;
    if (state.pendingRtcTracks.length >= PENDING_RTC_TRACKS_MAX) { ev.track.stop(); return; }
    state.pendingRtcTracks.push({ event: ev, generation });
  };
  candidate.onconnectionstatechange = () => env.handleConnectionState(generation, candidate, candidate.connectionState);
}

// Sends the offer through the encrypted control channel. onSent runs once it went out, onFailed with
// the error when creating or applying it failed for the current peer.
async function sendOffer(env, candidate, generation, { onSent, onFailed }) {
  try {
    const offer = await candidate.createOffer();
    if (!env.isCurrentPeer(generation, candidate)) return;
    await candidate.setLocalDescription(offer);
    if (!env.isCurrentPeer(generation, candidate)) return;
    if (env.sendControl({ type: MSG_OFFER, sdp: offer.sdp })) {
      onSent();
      env.state.rtcOfferSignaled = true;
      env.startFirstFrameHandshake(HANDSHAKE_OFFER_SENT);
    }
  } catch (error) {
    if (env.isCurrentPeer(generation, candidate)) onFailed(error);
  }
}

function cleanupPeerState(env) {
  const state = env.state;
  for (const { event } of state.pendingRtcTracks) event.track.stop();
  state.pendingRtcTracks = [];
  env.resetFileDropPreview();
  state.fileDropPresentedGeneration = null;
  state.fileDropPresentedViewportRevision = null;
  state.closingPeer = true;
  env.releaseInput();
  if (state.usingWssMedia) env.stopWssMedia();
  env.detachRtcSound();
  env.clearWssTimers();
  state.authenticatedSession?.resetPeer();
  env.resetInputUi();
  state.inputReady = false;
  state.connected = false;
  try { state.dc?.close(); } catch { /* already closed */ }
  try { state.motionDc?.close(); } catch { /* already closed */ }
  state.dc = null;
  state.motionDc = null;
  try { state.pc?.close(); } catch { /* already closed */ }
  state.pc = null;
  state.closingPeer = false;
  env.releaseMediaPipeline();
}

// env: state (the session state), hudState, requestedTransport, iceTransportPolicy, useWebCodecs,
// codecSession, mobilePointerDevice, isCurrentPeer(generation, candidate), sendControl(message),
// updateInputReady(), handleCursorState(data), handleConnectionState(generation, candidate, state),
// onTrack(event, generation), startFirstFrameHandshake(reason), startupSnapshot(generation) ->
// { framesReceived, nativeFramesPresented }, switchToWss(reason), onRtcHealthy(),
// requestPointerLock(), markBad(), markGood(), releaseInput(send), recycleSignaling(reason),
// endRtcPeerOnly(reason), reconnectSignaling(reason), requestWssMediaStart(),
// invalidatePendingMonitorRequest(reason), resetFirstFrameHandshake(), renderDesktopState(),
// clearHostQuality(), resetFileDropPreview(), stopWssMedia(), detachRtcSound(), clearWssTimers(),
// resetInputUi(), releaseMediaPipeline(), viewerLog(level, event, detail), and optional
// createPeerConnection(options) and receiverCapabilities() (for tests).
export function createPeer(env) {
  const state = env.state;
  const newPeerConnection = env.createPeerConnection ?? (options => new RTCPeerConnection(options));
  const receiverCapabilities = env.receiverCapabilities ?? (() => RTCRtpReceiver.getCapabilities(MEDIA_VIDEO));
  let offerFailures = 0;

  // A failed offer restarts the peer, up to OFFER_FAILURE_LIMIT in a row (reset by a sent offer). After
  // that auto mode moves to WSS; a forced WebRTC page shows the error instead of looping.
  function offerFailed(error) {
    offerFailures++;
    if (offerFailures < OFFER_FAILURE_LIMIT) { restart(`offer:${error?.message || error}`); return; }
    env.viewerLog("error", "offer-failed-giving-up", { failures: offerFailures, error: String(error?.message || error) });
    if (env.requestedTransport === TRANSPORT.AUTO) env.switchToWss(REASON_OFFER_FAILED);
    else { env.hudState.ice = REASON_OFFER_FAILED; env.markBad(); }
  }

  async function start(generation = state.peerGeneration) {
    if (env.requestedTransport === TRANSPORT.WSS || state.usingWssMedia) return;
    if (!state.authenticatedSession?.controlReady) return;
    if (state.offerSent || generation !== state.peerGeneration || !socketOpen(state)) return;
    state.offerSent = true;
    state.lastIceCandidateError = null;
    const peerOptions = { iceServers: state.currentIceServers, iceTransportPolicy: env.iceTransportPolicy };
    // This must be set during construction, but only the opt-in WebCodecs path
    // needs it. The native path keeps browser decode/render untouched.
    if (env.useWebCodecs) peerOptions.encodedInsertableStreams = true;
    const candidate = openPeerConnection(env, newPeerConnection, peerOptions);
    if (!candidate) {
      state.offerSent = false;
      constructionFailed(env);
      return;
    }
    state.pc = candidate;
    armStartupFallback(env, candidate, generation);
    const transceiver = addTransceivers(candidate);
    if (!applyCodecPreferences(env, candidate, transceiver, receiverCapabilities)) { void start(generation); return; }
    attachInputLanes(env, candidate, generation, restart);
    attachIceHandlers(env, candidate, generation);
    await sendOffer(env, candidate, generation, { onSent: () => { offerFailures = 0; }, onFailed: offerFailed });
  }

  function restart(reason, reconnect = true) {
    const generation = ++state.peerGeneration;
    env.invalidatePendingMonitorRequest(reason);
    env.resetFirstFrameHandshake();
    state.transitionNeedsKeyframe = false;
    state.transitionGatePassedToken = null;
    state.desktopAwaitingFrame = true;
    env.renderDesktopState();
    env.clearHostQuality();
    cleanupPeerState(env);
    state.pendingRemoteIce = [];
    state.remoteDescriptionSet = false;
    state.offerSent = false;
    state.rtcOfferSignaled = false;
    env.hudState.ice = reason;
    env.markBad();
    if (socketOpen(state)) {
      if (state.wssSelected) env.requestWssMediaStart();
      else void start(generation);
    } else if (reconnect) env.reconnectSignaling(reason);
  }

  return { start, cleanup: () => cleanupPeerState(env), restart };
}
