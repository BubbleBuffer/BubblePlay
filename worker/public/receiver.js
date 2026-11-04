import { RemoteClipboardWriter, sameClipboardImage, pasteHostClipboard } from "/remote-clipboard.js";
"use strict";
import { createLatencyGuard, decodeTimestampPatternBits, normalizeEncodedTimestamp, summarizeRtcStats, computeQosRxWindow, createQosPayload, parseHostQuality } from "/latency.js";
import { STUN_FALLBACK, deriveCandidateRoute, parseIceTransportPolicy, validateIceServers, wantsTurn } from "/turn.js";
import {
  shouldRecoverAfterNetworkRouteProbe,
  shouldReconnectAfterClose,
  signalingUnanswered,
  rtcPeerEndAction,
} from "/recovery.js";
import { ClipboardReceiver, keyDescriptor, WheelAccumulator } from "/input.js";
import { BrowserCursorAppearance } from "/cursor.js";
 import { canMoveAbsoluteCursor, touchTranslateWheel, CursorReconciler, decodeCursorState, InputController, reconcileTextEdit, RemoteCursor, secureTextUnits, shouldCaptureKeyboard, shouldCenterCameraOnCursor, shouldCorrectCursorDrift, shouldShowCursorOverlay, shouldShowPredictedCursor, TouchGesture, Viewport } from "/input.js";
import { CODEC, createRtcStartupFallback, createSignalingRecycleDiagnostic, drawBaseThenHybrid, FramePresentationTelemetry, HevcTileCache, HybridTileCache, prepareHybridWssFrame, presentWssFrameImmediately, probeHevcTileDecoderSupport } from "/media.js";
import { cropGain, cropNeedsUpdate, desiredSharpCrop, fullDesktopCrop } from "/sharp-viewport.js";
import { V4ReceiveGate, V4_HEARTBEAT_MS, V4_VERSION, ackMessageV4, detectAnnexBCodec, keyframeMessageV4, nextBackoffMsV4, parseFrameV4, shouldPresentDecodedV4, wssV4StatusLabel } from "/media-v4.js";
import {
  TRANSPORT, createCodecSession, forgetRtcFailure, parseQualityPreset, parseTransport, rememberRtcFailure,
  orderRtpCodecPreferences, rememberedRtcFailure, rtcFailureStorage, rtpCodecMimeTypes, selectWssDecoderPath,
} from "/transport.js";
import { createWssWorkerClient, wssWorkerSupported } from "/wss-worker-client.js";
import { avcCodecFromSps } from "/wss-pipeline.js";
import {
  desktopTransitionPlan, monitorRequestAfterRestart, monitorRequestReadiness, restartPeerInNewEpoch,
  rtcAdmissionGate, rtcGateOpens, rtcInPlaceCapable, rtpTimestampAtOrAfter, transitionKeyframeVerdict,
} from "/monitor-selection.js";
import { chordLabel, chordSteps, createEdgeReveal, createPopoverController, keyName, shortcutsFor } from "/chrome.js";
import { loadSettings, qualityStorageView, saveSettings } from "/settings.js";
// File transfer hook (1/5): client + minimal UI; see the "file transfer" section.
import { SoundController, WssAudioPlayer, isAudioPacket, probeOpusDecode, soundButtonView, sumAudioStats } from "/audio.js";
import { FileTransferClient } from "/file-client.js";
import { touchPc } from "/devicekey-store.js";
import { pcListUrl, registerProof, relayOrigin, viewerHello } from "/dk-relay.js";
import { b64u } from "/dk-crypto.js";
import { createViewerSession } from "/dk-session.js";
import { createLogUploader } from "/dk-upload.js";
import { createFileDragPreviewController, filesFromPaste, installFileTransferUi, mapFileDropPoint } from "/file-ui.js";

const debugTrace = (stage, detail = {}) => window.__bubbleplayDebugTrace?.trace(stage, detail);
const diagnosticAttemptId = window.__bubbleplayDebugTrace?.attemptId || null;

// ---------------------------------------------------------------------------
// BubblePlay browser client. Native WebRTC video is the normal display path;
// WebCodecs remains an explicit diagnostic/experimental opt-in.
//
// Flow:
//   0. dk-app.js unlocks the device key and calls startSession({ identity, pc, gate }).
//   1. Open the gate-cookie protected /ws?pc=<id>, send the signed viewer hello
//      (role=viewer), answer register-challenge with register-proof.
//   2. On hello-ok: create RTCPeerConnection with one recvonly video
//      transceiver (H.264) + one unordered/unreliable input DataChannel.
//   3. createOffer -> send to host via the relay. Host answers.
//   4. Incoming video: native <video> decode/render by default. With
//      ?decoderPath=webcodecs, insertable streams -> VideoDecoder (Annex B
//      H.264) -> desynchronized 2D canvas, latest-frame-wins pacing.
//   5. Pointer-lock mouse + keyboard -> binary little-endian packets on the
//      DataChannel, matching host/src/input.rs.
// ---------------------------------------------------------------------------

let screen = document.getElementById("screen");
let sharpScreen = document.getElementById("sharp-screen");
const stage = document.getElementById("stage");
const visual = document.getElementById("visual");
const inputSurface = document.getElementById("input-surface");
const hud = document.getElementById("hud");
const connectionStatus = document.getElementById("connection-status");
const connectionStatusLabel = document.getElementById("connection-status-label");
const secureDesktopStatus = document.getElementById("secure-desktop-status");
const diagnosticsOutput = document.getElementById("diagnostics-output");
// Do not create a main-thread context until the legacy path is selected:
// transferControlToOffscreen is only legal before a rendering context exists.
let ctx = null;
let sharpCtx = null;
let lastDecodedDimensions = null;
function ensureMainCanvas() {
  if (!ctx) ctx = screen.getContext("2d", { alpha: false, desynchronized: true });
  return ctx;
}
function ensureSharpCanvas() {
  if (!sharpCtx) sharpCtx = sharpScreen.getContext("2d", { alpha: false, desynchronized: true });
  return sharpCtx;
}

function replaceTransferredCanvas() {
  const replacement = document.createElement("canvas");
  replacement.id = "screen";
  screen.replaceWith(replacement);
  screen = replacement;
  ctx = null;
}

function replaceTransferredSharpCanvas() {
  const replacement = document.createElement("canvas");
  replacement.id = "sharp-screen";
  sharpScreen.replaceWith(replacement);
  sharpScreen = replacement;
  sharpCtx = null;
}

function releaseWebRtcVideoSink() {
  try { decoder?.close(); } catch {}
  decoder = null;
  if (encodedWorker) {
    encodedWorker.terminate();
    encodedWorker = null;
    replaceTransferredCanvas();
  }
  encodedWorkerStats = null;
  activeVideo?.remove();
  activeVideo = null;
  nativePresentation = null;
}

const params = new URLSearchParams(location.search);
// Media-control messages carry the WSS protocol version (v4 is the only one).
const WSS_PROTOCOL = V4_VERSION;
// Opus decode is probed once. The WSS start opts in to BPA1 audio only when it can decode.
const wssAudioDecodable = probeOpusDecode(globalThis.AudioDecoder);
const useWebCodecs = params.get("decoderPath") === "webcodecs";
const requestedTransport = parseTransport(params);
function browserStorage() {
  try { return window.localStorage; } catch { return null; }
}
// ?transport= always wins; only auto mode consults the remembered RTC failure.
const startOnRememberedWss = rememberedRtcFailure(requestedTransport, rtcFailureStorage());
// One versioned settings record (settings.js); ?quality= still wins for a page.
const settings = loadSettings(browserStorage());
function persistSettings() { saveSettings(browserStorage(), settings); }
let qualityPreset = parseQualityPreset(params, qualityStorageView(settings));
const codecSession = createCodecSession({ transport: requestedTransport, decoderPath: useWebCodecs ? "webcodecs" : "native" }); // page-local
const diagnosticsEnabled = params.get("diag") === "1";
const profilingEnabled = params.get("profile") === "1" || diagnosticsEnabled;
debugTrace("module-ready", {
  transport: requestedTransport,
  decoderPath: useWebCodecs ? "webcodecs" : "native",
});
const fullFrameDiagnostic = params.get("fullFrame") === "1";
const iceTransportPolicy = parseIceTransportPolicy(params);
const profileSessionId = crypto.randomUUID?.() || `${Date.now()}-${Math.random().toString(16).slice(2)}`;
let profileSequence = 0;
let qosSequence = 0;
let latestRtcProfile = null;
let latestRtcProfileGeneration = null;
let hostQuality = null;
let latestBenchStatus = null;
const sharpZoomEnabled = !fullFrameDiagnostic && params.get("sharpZoom") !== "0";
const desktopState = document.getElementById("desktop-state");
const desktopStateTitle = document.getElementById("desktop-state-title");
const desktopStateDetail = document.getElementById("desktop-state-detail");
const firstFrameRetry = document.getElementById("first-frame-retry");
const secureDeny = document.getElementById("secure-deny");
let desktopAvailable = true;
let desktopAwaitingFrame = false;
let suppressDesktopTransitionOverlay = false;
let transitionNeedsKeyframe = false;
// requestId whose post-admission keyframe passed the main-thread decode gate.
let transitionGatePassedToken = null;
let desktopKind = "default";
// Secure desktop (UAC prompt, lock screen): what it is and what the host's
// SYSTEM helper lets this viewer do there ("view", "deny", "input").
let secureKind = "other";
let secureCapabilities = [];
let secureDenyState = { pending: false, error: "" };
let desktopInputState = "enabled";
let desktopGeneration = null;
let latestInputStatus = null;
let inputStatusReason = null;
let hostCapabilities = { serviceMode: false, coldBoot: false, secureDesktop: false, secureText: false };
let hostCapabilitiesGeneration = 0;
// Local PC sound (audio.js). Sinks: the WebRTC <audio> element and the WSS player.
let hostAudioCapable = false; // host-capabilities.audio === 1
let wssAudioPlayer = null;
let wssAudioSinkAttached = false;
let wssAudioRetired = sumAudioStats(); // counters from players already stopped
let latestInboundAudio = null; // WebRTC inbound-rtp for the audio track
const soundController = new SoundController({ target: document, onChange: () => renderSoundSlot() });
let hasPresentedFrame = false;
let fileDropPresentedGeneration = null;
let fileDropPresentedViewportRevision = null;
// rtpGate: on WebRTC, how presentation of the admitted generation is proven
// (rtcAdmissionGate in monitor-selection.js); null keeps the legacy rule.
const firstFrameHandshake = { requestId: null, reason: "", transport: null, epoch: null, generation: 0, phase: "idle", retries: 0, admitted: false, presented: false, requireAdmission: false, rtpGate: null, failureReason: "", timer: null };
let viewportRevision = 0;
let viewportTimer = null;
let viewportLast = null;
let viewportState = { enabled: false, revision: 0, generation: 0 };
let viewportPending = null;
let viewportDesired = null;
let viewportTransitionTimer = null;
let viewportLastSentAt = -Infinity;
let viewportConsecutiveTimeouts = 0;
let sharpDisabledForSession = false;
let renderedSharpView = null;
let viewportTelemetry = { state: "full", revision: 0, generation: 0, crop: "1920x1080@0,0" };
const clientBuild = "native-file-drop-2026-10-05";

function secureDesktopText() {
  const title = secureKind === "uac" ? "Admin prompt open on the PC" : secureKind === "lock" ? "PC is locked" : "Secure screen open on the PC";
  if (secureDenyState.pending) return [title, "Closing the prompt…"];
  if (secureDenyState.error) return [title, `Couldn't close the prompt (${secureDenyState.error}).`];
  if (!secureCapabilities.includes("view")) return [title, "BubblePlay can't show this screen. The stream resumes when it closes."];
  if (secureCapabilities.includes("input")) return [title, secureKind === "lock" ? "You can sign in with the keyboard." : "You can use it with mouse and keyboard."];
  return [title, secureKind === "uac" ? "View only. Turn on Admin mode on the PC to approve prompts." : "View only. Turn on Admin mode on the PC to use it."];
}
function renderDesktopState() {
  document.body.classList.toggle("desktop-unavailable", !desktopAvailable);
  if (desktopKind === "secure" && !(desktopAvailable && desktopAwaitingFrame && !hasPresentedFrame)) {
    const [title, detail] = secureDesktopText();
    desktopState.dataset.state = "secure";
    desktopState.hidden = false;
    desktopStateTitle.textContent = title;
    desktopStateDetail.textContent = detail;
    secureDeny.hidden = !secureCapabilities.includes("deny");
    secureDeny.disabled = secureDenyState.pending;
    return;
  }
  secureDeny.hidden = true;
  const state = !desktopAvailable ? "unavailable" : desktopAwaitingFrame ? (firstFrameHandshake.phase === "failed" ? "failed" : firstFrameHandshake.phase === "retrying" ? "retrying" : "starting") : "ready";
  desktopState.dataset.state = state;
  desktopState.hidden = state === "ready" || (suppressDesktopTransitionOverlay && hasPresentedFrame && desktopAwaitingFrame);
  desktopStateTitle.textContent = state === "unavailable" ? "Windows desktop unavailable" : state === "failed" ? (hasPresentedFrame ? "Stream paused" : "Stream did not start") : state === "retrying" ? (hasPresentedFrame ? "Reconnecting…" : "Still starting the stream…") : (hasPresentedFrame ? "Refreshing stream…" : "Starting stream…");
  desktopStateDetail.textContent = state === "unavailable" ? (hasPresentedFrame ? "The last frame remains visible. Input is released until the desktop returns." : "The secure or locked desktop cannot be captured right now.") : state === "failed" ? `${hasPresentedFrame ? "The last frame remains visible; " : ""}no new frame arrived${firstFrameHandshake.failureReason ? ` (${firstFrameHandshake.failureReason})` : ""}.` : state === "retrying" ? "Input is released while BubblePlay rebuilds the media path." : firstFrameHandshake.phase === "in-flight" ? "The host admitted a keyframe; waiting for browser presentation." : (hasPresentedFrame ? "The last frame remains visible while a fresh keyframe arrives." : "Waiting for the first visible frame.");
}
function handleDesktopState(message) {
  if (message?.state !== "unavailable" && message?.state !== "active") return;
  const desktopGenerationChanged = Number.isInteger(message.generation) &&
    message.generation !== desktopGeneration;
  if (Number.isInteger(message.generation)) desktopGeneration = message.generation;
  if (["default", "secure", "none"].includes(message.desktop)) desktopKind = message.desktop;
  if (desktopKind === "secure") {
    const nextKind = ["uac", "lock", "other"].includes(message.secureKind) ? message.secureKind : "other";
    if (nextKind !== secureKind) secureDenyState = { pending: false, error: "" };
    secureKind = nextKind;
    secureCapabilities = Array.isArray(message.secureCapabilities)
      ? message.secureCapabilities.filter(c => ["view", "deny", "input"].includes(c)) : [];
  } else {
    secureKind = "other";
    secureCapabilities = [];
    secureDenyState = { pending: false, error: "" };
  }
  if (["enabled", "suspended", "read-only"].includes(message.input)) desktopInputState = message.input;
  if (desktopInputState !== "enabled") releaseInput(false);
  const secureDesktop = desktopKind === "secure";
  if (secureDesktop) clearRemoteClipboard();
  document.body.classList.toggle("secure-desktop", secureDesktop);
  secureDesktopStatus.hidden = !secureDesktop;
  keyboardInput.setAttribute("autocorrect", secureDesktop ? "off" : "on");
  keyboardInput.setAttribute("autocomplete", secureDesktop ? "off" : "on");
  keyboardInput.setAttribute("autocapitalize", secureDesktop ? "off" : "sentences");
  keyboardInput.spellcheck = !secureDesktop;
  if (secureDesktop) keyboardStatus.textContent = desktopInputState === "enabled" ? "Protected typing is active. Paste and autocorrect are disabled." : "Secure desktop is read-only.";
  else if (keyboardStatus.textContent.startsWith("Protected") || keyboardStatus.textContent.includes("secure desktop")) keyboardStatus.textContent = "Native text input is ready.";
  // Monitor changes intentionally announce an unavailable/active pair while
  // the host rebuilds capture and emits the new-generation IDR. The old frame
  // is still a valid visible desktop during that short transition, so do not
  // replace it with the global startup card or restart the handshake.
  // Both transports switch monitors in place on the existing peer. Held keys
  // and buttons survive (the edge-hold drag carries a held window); input is
  // gated until the new generation presents, and release-all follows only if
  // something held was let go meanwhile (InputController.resumeAfterTransition).
  const alreadyRendering = hudState.presented > 0 || framesDrawn > 0 || firstFrameHandshake.phase === "ready" || firstFrameHandshake.phase === "presented";
  const transitionPlan = desktopTransitionPlan({ reason: message.reason, state: message.state, usingWss: usingWssMedia, alreadyRendering, rtcInPlace: rtcInPlaceTransitionCapable() });
  const monitorTransition = transitionPlan?.kind === "monitor";
  const sourceTransition = transitionPlan?.kind === "source";
  if (monitorTransition && alreadyRendering) {
    desktopAvailable = true;
    desktopAwaitingFrame = true;
    suppressDesktopTransitionOverlay = true;
    if (transitionPlan.action === "restart-peer") {
      // This browser cannot prove the new monitor's frames on WebRTC.
      releaseInput(false);
      renderDesktopState();
      restartRtcForTransition("monitor-transition");
      return;
    }
    inputTransitionHold = true;
    if (motionFrame !== null) cancelAnimationFrame(motionFrame);
    motionFrame = null;
    renderDesktopState();
    if (transitionPlan.action === "handshake") startFirstFrameHandshake("monitor-transition", true);
    return;
  }
  // Keep the last decoded frame visible across UAC/Winlogon/default desktop
  // changes, but never treat it as proof that the newly selected desktop is
  // ready for input. Only presentation from the new handshake clears this
  // gate in markFirstFramePresented().
  if (sourceTransition && alreadyRendering) {
    desktopAvailable = true;
    desktopAwaitingFrame = true;
    suppressDesktopTransitionOverlay = false;
    releaseInput(false);
    renderDesktopState();
    // WSS carries an explicit generation. On WebRTC the admission carries the
    // new IDR's RTP timestamp and presentation must reach it (rtpGate); a
    // browser that cannot check that recreates the complete receiver epoch so
    // no jitter buffer or worker can authorize Winlogon/default input with an
    // old frame.
    if (transitionPlan.action === "restart-peer") {
      restartRtcForTransition("desktop-source-transition");
      return;
    }
    startFirstFrameHandshake("desktop-source-transition", true);
    return;
  }
  desktopAvailable = message.state === "active";
  desktopAwaitingFrame = true;
  suppressDesktopTransitionOverlay = false;
  if (!desktopAvailable) {
    clearFirstFrameWatchdog();
    firstFrameHandshake.phase = "idle";
  }
  releaseInput(false);
  if (!desktopAvailable) sharpScreen.classList.remove("active");
  renderDesktopState();
  // desktopAwaitingFrame is now set, so a handshake that already presented
  // (or ended) must be replaced; only one still in flight is reused.
  if (desktopAvailable) startFirstFrameHandshake("desktop-state", desktopStateNeedsNewHandshake(desktopGenerationChanged));
}
function clearFirstFrameWatchdog() {
  if (firstFrameHandshake.timer !== null) clearTimeout(firstFrameHandshake.timer);
  firstFrameHandshake.timer = null;
}
function firstFrameRequestId() {
  const bytes = new Uint8Array(12);
  crypto.getRandomValues(bytes);
  return btoa(String.fromCharCode(...bytes)).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}
function sendFirstFrameRequest() {
  if (!firstFrameHandshake.requestId || ws?.readyState !== WebSocket.OPEN) return false;
  if (!sendControl({ type: "first-frame", protocol: 1, phase: "request", requestId: firstFrameHandshake.requestId, transport: firstFrameHandshake.transport, ...(firstFrameHandshake.epoch ? { epoch: firstFrameHandshake.epoch } : {}) })) return false;
  if (firstFrameHandshake.transport === "webrtc") rtcStartupFallback?.requestSent();
  return true;
}
function armFirstFrameWatchdog() {
  clearFirstFrameWatchdog();
  const requestId = firstFrameHandshake.requestId;
  firstFrameHandshake.timer = setTimeout(() => {
    if (requestId !== firstFrameHandshake.requestId || firstFrameHandshake.presented) return;
    if (firstFrameHandshake.retries < 1) {
      firstFrameHandshake.retries++;
      firstFrameHandshake.phase = "retrying";
      renderDesktopState();
      sendFirstFrameRequest();
      sendControl({ type: "media-control", action: "keyframe", protocol: WSS_PROTOCOL, epoch: firstFrameHandshake.epoch, reason: "first-frame-timeout", codec: codecSession.codec });
      armFirstFrameWatchdog();
    } else {
      firstFrameHandshake.phase = "failed";
      firstFrameHandshake.failureReason = "presentation-timeout";
      firstFrameHandshake.timer = null;
      renderDesktopState();
      recoverWssStartup("presentation-timeout");
      recoverRtcInPlaceTransition("presentation-timeout");
    }
  }, 5_000);
}
function startFirstFrameHandshake(reason, forceNew = false) {
  if (ws?.readyState !== WebSocket.OPEN) return;
  if (!usingWssMedia && !rtcOfferSignaled) return;
  const transport = usingWssMedia ? "wss" : "webrtc";
  const epoch = usingWssMedia ? mediaEpoch : null;
  if (consumeRtcTransitionRestart()) forceNew = true;
  if (!forceNew && firstFrameHandshake.requestId && firstFrameHandshake.transport === transport && firstFrameHandshake.epoch === epoch && firstFrameHandshake.phase !== "failed") return;
  clearFirstFrameWatchdog();
  firstFrameHandshake.requestId = firstFrameRequestId();
  firstFrameHandshake.transport = transport;
  firstFrameHandshake.epoch = epoch;
  firstFrameHandshake.generation = 0;
  firstFrameHandshake.phase = "starting";
  firstFrameHandshake.retries = 0;
  firstFrameHandshake.admitted = false;
  firstFrameHandshake.presented = false;
  transitionNeedsKeyframe = false;
  firstFrameHandshake.requireAdmission = forceNew || reason === "desktop-source-transition" || reason === "monitor-transition";
  firstFrameHandshake.rtpGate = null;
  transitionNeedsKeyframe = firstFrameHandshake.requireAdmission && !usingWssMedia && useWebCodecs;
  firstFrameHandshake.failureReason = "";
  firstFrameHandshake.reason = reason;
  desktopAvailable = true;
  desktopAwaitingFrame = true;
  renderDesktopState();
  sendFirstFrameRequest();
  viewerLog("info", "first-frame-request", { reason, transport });
  armFirstFrameWatchdog();
}
// An in-place WebRTC monitor or desktop-source transition that cannot be
// proven, or never presents, falls back to the full receiver-epoch restart
// instead of leaving the stream paused. Input stays gated: restartPeer keeps
// desktopAwaitingFrame until the new peer's own handshake presents.
const RTC_IN_PLACE_TRANSITIONS = ["monitor-transition", "desktop-source-transition"];
function recoverRtcInPlaceTransition(reason) {
  if (usingWssMedia || !RTC_IN_PLACE_TRANSITIONS.includes(firstFrameHandshake.reason)) return;
  const transition = firstFrameHandshake.reason;
  viewerLog("warn", `${transition}-restart`, { reason });
  restartRtcForTransition(transition);
}
// A transition that fell back to a peer restart still needs proof: the new
// peer's first handshake is forced and requires admission, and it is gated on
// the admitted RTP timestamp when the host sends one (as a startup handshake,
// so a missing timestamp cannot loop into another restart).
let rtcTransitionRestartPending = false;
function restartRtcForTransition(transition) {
  rtcTransitionRestartPending = true;
  restartPeerInNewEpoch({ recycleSignaling, restartPeer }, transition);
}
function consumeRtcTransitionRestart() {
  if (!rtcTransitionRestartPending) return false;
  rtcTransitionRestartPending = false;
  return !usingWssMedia;
}
// A desktop-state message re-gates input; reuse only a handshake in flight.
const FIRST_FRAME_IN_FLIGHT = ["starting", "retrying", "in-flight"];
function desktopStateNeedsNewHandshake(generationChanged) {
  return Boolean(generationChanged) || !firstFrameHandshake.requestId || !FIRST_FRAME_IN_FLIGHT.includes(firstFrameHandshake.phase);
}
// Which WebRTC media path presents frames on this peer.
function rtcMediaPath() {
  if (activeVideo) return "native";
  if (encodedWorker) return "worker";
  return useWebCodecs ? "decoder" : "native";
}
function rtcNativeProofSupport() {
  const current = nativePresentation?.generation === peerGeneration ? nativePresentation : null;
  return { rvfc: Boolean(current?.supported), rtpTimestamps: Boolean(current?.rtpTimestamps) };
}
function rtcInPlaceTransitionCapable() {
  if (usingWssMedia) return false;
  return rtcInPlaceCapable({ path: rtcMediaPath(), ...rtcNativeProofSupport() });
}
// proof: { rtpTimestamp } from a native presented frame, or { gatePassed } from
// a WebCodecs path that already dropped frames until the admitted keyframe.
function markFirstFramePresented(presentedGeneration = null, presentedViewportRevision = null, proof = null) {
  if (!firstFrameHandshake.presented && firstFrameHandshake.requireAdmission && !firstFrameHandshake.admitted) return false;
  if (!firstFrameHandshake.presented && firstFrameHandshake.requireAdmission && !usingWssMedia
      && !rtcGateOpens(firstFrameHandshake.rtpGate, proof)) return false;
  if (!firstFrameHandshake.presented && firstFrameHandshake.requireAdmission
      && Number.isInteger(presentedGeneration)
      && Number.isInteger(firstFrameHandshake.generation)
      && firstFrameHandshake.generation !== 0
      && presentedGeneration !== firstFrameHandshake.generation) return false;
  if (Number.isInteger(presentedGeneration)) fileDropPresentedGeneration = presentedGeneration;
  if (Number.isInteger(presentedGeneration)) fileDropPresentedViewportRevision = Number.isInteger(presentedViewportRevision) ? presentedViewportRevision : null;
  if (firstFrameHandshake.presented) return true;
  if (usingWssMedia) wssStartupRecoveries = 0;
  firstFrameHandshake.presented = true;
  if (!usingWssMedia) {
    rtcStartupFallback?.stop();
    forgetRtcFailure(rtcFailureStorage());
  }
  hasPresentedFrame = true;
  document.body.classList.add("has-presented-frame");
  firstFrameHandshake.phase = "presented";
  debugTrace("first-presented", {
    transport: usingWssMedia ? "wss" : "webrtc",
    generation: Number.isInteger(presentedGeneration) ? presentedGeneration : 0,
  });
  clearFirstFrameWatchdog();
  desktopAwaitingFrame = false;
  suppressDesktopTransitionOverlay = false;
  if (inputTransitionHold) {
    inputTransitionHold = false;
    input.resumeAfterTransition();
    if (inputReady) onInputReady();
  }
  renderDesktopState();
  showContextHint();
  if (firstFrameHandshake.admitted && firstFrameHandshake.requestId) {
    sendControl({ type: "first-frame", protocol: 1, phase: "presented", requestId: firstFrameHandshake.requestId, generation: firstFrameHandshake.generation });
  }
  return true;
}
function handleFirstFrameMessage(message) {
  if (!message || message.protocol !== 1 || message.requestId !== firstFrameHandshake.requestId) return;
  if (message.phase === "admitted") {
    const firstAdmission = !firstFrameHandshake.admitted;
    if (firstAdmission && !usingWssMedia) rtcStartupFallback?.progress();
    if (firstAdmission && !usingWssMedia && firstFrameHandshake.requireAdmission) {
      const plan = rtcAdmissionGate({
        transition: RTC_IN_PLACE_TRANSITIONS.includes(firstFrameHandshake.reason),
        path: rtcMediaPath(), admittedRtpTimestamp: message.rtpTimestamp, ...rtcNativeProofSupport(),
      });
      if (plan.action === "restart-peer") {
        recoverRtcInPlaceTransition(plan.reason);
        return;
      }
      firstFrameHandshake.rtpGate = plan.action === "gate" ? { ...plan.gate, token: message.requestId } : null;
    }
    firstFrameHandshake.admitted = true;
    firstFrameHandshake.generation = Number.isInteger(message.generation) ? message.generation : firstFrameHandshake.generation;
    firstFrameHandshake.phase = "in-flight";
    if (firstAdmission && firstFrameHandshake.requireAdmission && !usingWssMedia && useWebCodecs) {
      transitionNeedsKeyframe = true;
      transitionGatePassedToken = null;
      resyncPipeline("desktop-transition-admitted", firstFrameHandshake.rtpGate);
    }
    // A native frame may have been presented before this message arrived over
    // signaling; it counts only if it reached the admitted IDR.
    if (firstAdmission && firstFrameHandshake.rtpGate?.mode === "native"
        && rtpTimestampAtOrAfter(nativePresentation?.lastRtpTimestamp, firstFrameHandshake.rtpGate.admittedRtpTimestamp)) {
      markFirstFramePresented(null, null, { rtpTimestamp: nativePresentation.lastRtpTimestamp });
    }
    renderDesktopState();
    if (firstFrameHandshake.presented) sendControl({ type: "first-frame", protocol: 1, phase: "presented", requestId: message.requestId, generation: firstFrameHandshake.generation });
  } else if (message.phase === "failed") {
    if (firstFrameHandshake.presented) {
      // Visible output from this request is stronger evidence than a delayed
      // host watchdog failure. Never recycle a healthy stream or resurrect
      // the blocking startup card after presentation.
      clearFirstFrameWatchdog();
      firstFrameHandshake.phase = "presented";
      firstFrameHandshake.failureReason = "";
      desktopAwaitingFrame = false;
      renderDesktopState();
      viewerLog("warn", "first-frame-failure-ignored", {
        reason: String(message.reason || "host-failure").slice(0, 96),
        generation: message.generation ?? null,
      });
      return;
    }
    firstFrameHandshake.phase = "failed";
    firstFrameHandshake.failureReason = String(message.reason || "host-failure").slice(0, 96);
    clearFirstFrameWatchdog();
    renderDesktopState();
    recoverWssStartup(firstFrameHandshake.failureReason);
    recoverRtcInPlaceTransition(firstFrameHandshake.failureReason);
  } else if (message.phase === "ready") {
    firstFrameHandshake.phase = "ready";
    clearFirstFrameWatchdog();
  }
}
let monitorList = [];
let monitorListRevision = 0;
let monitorSwitchPending = null;
let lastMonitorRequest = null;
let lastMonitorError = null;
let monitorErrorNotice = null; // shown in the Monitors popover until the next list
let hostFollowCapable = false; // host-capabilities.followWindow >= 1
let hostFollowState = null; // monitor-list.follow, the host's truth when known
function invalidatePendingMonitorRequest(reason) {
  const invalidated = monitorRequestAfterRestart(monitorSwitchPending, reason);
  if (!invalidated) return;
  monitorSwitchPending = null;
  viewerLog("warn", "monitor-request-invalidated", invalidated);
  renderMonitorList();
}
function renderMonitorList() {
  const list = document.getElementById("monitor-list");
  const status = document.getElementById("monitor-status");
  if (!list || !status) return;
  list.replaceChildren();
  renderMonitorChrome();
  renderFollowWindow();
  if (!monitorList.length) { status.textContent = monitorErrorNotice ? `Monitor switch failed: ${monitorErrorNotice}` : "Waiting for monitor information…"; return; }
  status.textContent = monitorSwitchPending ? "Switching display…" : monitorErrorNotice ? `Monitor switch failed: ${monitorErrorNotice}` : monitorList.length === 1 ? "The host has one monitor." : "";
  for (const monitor of monitorList) {
    const button = document.createElement("button");
    button.type = "button";
    button.className = "pop-row monitor-row";
    button.setAttribute("role", "radio");
    button.setAttribute("aria-checked", String(monitor.selected));
    button.disabled = !!monitorSwitchPending;
    button.innerHTML = monitor.selected ? '<svg class="pop-row-mark" viewBox="0 0 24 24" aria-hidden="true"><path d="m5 12.5 4.5 4.5L19 7.5"/></svg>' : "";
    const title = document.createElement("span");
    title.className = "pop-row-title";
    title.textContent = monitor.name || monitor.selector;
    const detail = document.createElement("span");
    detail.className = "pop-row-detail";
    detail.textContent = `${monitor.width || "?"}×${monitor.height || "?"}`;
    const trail = document.createElement("span");
    trail.className = "pop-row-trail";
    trail.textContent = monitor.selected ? "Current" : monitorSwitchPending === monitor.selector ? "Switching…" : "";
    button.append(title, detail, trail);
    button.onclick = () => {
      if (!monitor.selected) requestMonitor(monitor.selector);
      popovers.close();
    };
    list.append(button);
  }
}
// Strip icon state and the Settings › Display default-monitor choices.
function renderMonitorChrome() {
  const stripButton = document.getElementById("strip-monitors");
  const single = monitorList.length === 1;
  stripButton.disabled = single;
  const label = single ? "Monitors: the host has one monitor" : "Monitors";
  stripButton.setAttribute("aria-label", label);
  stripButton.title = label;
  const select = document.getElementById("setting-default-monitor");
  const options = [["", "Host's choice"], ...monitorList.map(m => [m.selector, m.name || m.selector])];
  if (settings.defaultMonitor && !monitorList.some(m => m.selector === settings.defaultMonitor))
    options.push([settings.defaultMonitor, "Saved monitor (not connected)"]);
  select.replaceChildren(...options.map(([value, text]) => new Option(text, value)));
  select.value = settings.defaultMonitor;
}
// A saved default monitor is requested once per page, on the first list.
let defaultMonitorApplied = false;
function applyDefaultMonitor() {
  if (defaultMonitorApplied || !settings.defaultMonitor || !monitorList.length) return;
  defaultMonitorApplied = true;
  const target = monitorList.find(m => m.selector === settings.defaultMonitor);
  if (target && !target.selected) requestMonitor(target.selector);
}
function handleMonitorList(message) {
  if (!Array.isArray(message.monitors) || message.monitors.length > 16) return;
  monitorList = message.monitors.filter(m => m && typeof m.selector === "string" && m.selector.length <= 128)
    .map(m => ({ selector: m.selector, name: typeof m.name === "string" ? m.name : m.selector, width: Number(m.width) || 0, height: Number(m.height) || 0, selected: !!m.selected }));
  monitorListRevision++;
  if (typeof message.follow === "boolean") hostFollowState = message.follow;
  const selected = monitorList.find(m => m.selected);
  const sessionDisplay = document.getElementById("session-display");
  if (sessionDisplay) sessionDisplay.textContent = selected ? `Now showing ${selected.name}` : "No display selected";
  if (selected?.width > 0 && selected?.height > 0 && typeof remoteDesktop !== "undefined") {
    remoteDesktop.width = selected.width; remoteDesktop.height = selected.height;
    syncViewportAndFollow?.();
  }
  monitorSwitchPending = null;
  monitorErrorNotice = null;
  renderMonitorList();
  applyDefaultMonitor();
}
function handleMonitorError(message) {
  monitorSwitchPending = null;
  lastMonitorError = {
    selector: typeof message.selector === "string" ? message.selector : null,
    reason: String(message.reason || "unknown error").slice(0, 180),
    atMs: Date.now(),
  };
  monitorErrorNotice = lastMonitorError.reason;
  renderMonitorList();
}
function requestMonitor(selector) {
  const readiness = monitorRequestReadiness({
    socketOpen: ws?.readyState === WebSocket.OPEN,
    signalingRegistered,
    pending: monitorSwitchPending,
  });
  if (!readiness.allowed) {
    if (readiness.reason !== "pending")
      viewerLog("info", "monitor-request-deferred", { reason: readiness.reason, selector });
    return false;
  }
  monitorSwitchPending = selector;
  lastMonitorRequest = { selector, atMs: Date.now() };
  renderMonitorList();
  sendControl({ type: "media-control", action: "monitor", protocol: WSS_PROTOCOL, epoch: mediaEpoch, monitor: selector, reason: "user-monitor" });
  return true;
}
// Follow active window (docs/architecture/follow-active-window.md): the host
// decides and switches through the normal desktop-state/monitor-list path;
// the client only owns the saved choice and resends it on every connect.
function followWindowShown() { return hostFollowState ?? settings.followWindow; }
function renderFollowWindow() {
  const group = document.getElementById("follow-window-group");
  const row = document.getElementById("follow-window");
  if (!group || !row) return;
  group.hidden = !hostFollowCapable;
  row.setAttribute("aria-checked", String(followWindowShown()));
  row.disabled = monitorList.length === 1;
}
function sendFollowWindow() {
  if (!hostFollowCapable) return;
  sendControl({ type: "media-control", action: "follow", follow: settings.followWindow, protocol: WSS_PROTOCOL, epoch: mediaEpoch, reason: "user-follow" });
}
function applyFollowCapability(message) {
  hostFollowCapable = Number.isInteger(message.followWindow) && message.followWindow >= 1;
  hostFollowState = null; // a new host session starts with follow off until told
  renderFollowWindow();
  if (settings.followWindow) sendFollowWindow();
}
function toggleFollowWindow() {
  if (!hostFollowCapable) return;
  settings.followWindow = !followWindowShown();
  persistSettings();
  hostFollowState = null; // show the choice until the host's next monitor-list
  renderFollowWindow();
  sendFollowWindow();
}

// ---- status HUD ------------------------------------------------------------
const hudState = { ws: "…", ice: "…", route: "…", dc: "…", control: "…", motion: "…", pointer: "desktop", cursorSeq: "…/…", cursorAge: "…", motionSent: 0, motionCoalesced: 0, motionDropped: 0, zoom: 1, touch: "…", dec: "…", fps: 0, recv: 0, queued: 0, dropped: 0, resyncs: 0, keyframes: 0, lag: "…", rtt: "…", jit: "…", jbuf: "…", loss: "…", mbps: "…", presented: 0, frameGap: "…", maxFrameGap: 0, dimensions: "…", mediaGeneration: "…" };
function renderConnectionStatus() {
  let state = "connecting";
  let label = "Connecting…";
  if (terminalSignalingFailure) { state = "error"; label = "Session unavailable"; }
  else if (!desktopAvailable) { state = "error"; label = "Desktop unavailable"; }
  else if (usingWssMedia) {
    // v4 never reports "waiting for stream" for a static desktop: no frames
    // is the expected steady state while nothing on screen changes.
    const v4 = wssV4Status();
    state = v4 === "connected" || v4 === "paused-static" ? "connected" : "connecting";
    label = v4 === "connected" ? "Connected · WSS" : v4 === "paused-static" ? "Connected · WSS (static)" :
      v4 === "reconnecting" ? "Reconnecting media…" : v4 === "starting" ? "Starting stream…" : "Connecting…";
  }
  else if (inputReady && usingWssMedia) { state = "connected"; label = "Connected · WSS"; }
  else if (inputReady && (hudState.ice === "connected" || hudState.ice === "completed" || hudState.ice === "connected/connected")) { state = "connected"; label = "Connected · WebRTC"; }
  else if (hudState.ws.startsWith("reconnecting") || hudState.ws === "closed") { label = "Reconnecting…"; }
  else if (hudState.ws === "error") { state = "error"; label = "Connection error"; }
  connectionStatusDetail = label.replace(" · ", " via ");
  // The strip shows a short state; transport and round trip go in the tooltip.
  const warning = desktopAvailable && desktopKind === "default" && !terminalSignalingFailure
    ? inputStatusReason === "admin-required" ? "This window needs Admin mode — turn it on at the PC"
      : inputStatusReason === "admin-unavailable" ? "Admin mode helper isn't responding" : "" : "";
  const shortLabel = warning || label.split(" · ")[0];
  const detail = [warning, connectionStatusDetail,
    hudState.rtt !== "…" ? `round trip ${hudState.rtt} ms` : null,
    monitorList.find(m => m.selected)?.name || null].filter(Boolean).join(" · ");
  if (connectionStatus.dataset.state !== state) connectionStatus.dataset.state = state;
  if (connectionStatusLabel.textContent !== shortLabel) connectionStatusLabel.textContent = shortLabel;
  if (connectionStatus.title !== detail) connectionStatus.title = detail;
  const retry = document.getElementById("connection-retry");
  if (retry.hidden !== (state !== "error")) retry.hidden = state !== "error";
  document.getElementById("session-strip").classList.toggle("status-error", state === "error");
  document.getElementById("session-strip").classList.toggle("status-warning", !!warning);
}
let connectionStatusDetail = "Connecting…";
function qualityHudText() {
  if (!hostQuality) return "quality …";
  const target = `${Math.round(hostQuality.targetBitrateBps / 1_000)}k`;
  const bwe = hostQuality.bweBps === null ? "…" : `${Math.round(hostQuality.bweBps / 1_000)}k`;
  return `quality ${hostQuality.profile} ${hostQuality.width}x${hostQuality.height}@${hostQuality.fps} ${target}/${bwe} ${hostQuality.reason}`;
}
setInterval(() => {
  if (cursorReconciler?.feedbackAt !== null && cursorReconciler?.feedbackAt !== undefined) hudState.cursorAge = Math.round(performance.now()-cursorReconciler.feedbackAt);
  hud.innerHTML =
    `ws ${hudState.ws} | ice ${hudState.ice} | route ${hudState.route} | control ${hudState.control} motion ${hudState.motion}\n` +
    `zoom ${hudState.zoom}× | pointer ${hudState.pointer} ${hudState.cursorSeq} ${hudState.cursorAge}ms | motion ${hudState.motionSent}/${hudState.motionCoalesced}/${hudState.motionDropped} | touch ${hudState.touch}\n` +
    `decoder ${hudState.dec} | recv ${hudState.recv} | queue ${hudState.queued} | drop ${hudState.dropped} | reset ${hudState.resyncs} | key ${hudState.keyframes} | ${hudState.fps} fps\n` +
    `presented ${hudState.presented} | gap ${hudState.frameGap}/${hudState.maxFrameGap}ms | ${hudState.dimensions}\n` +
    `lag ${hudState.lag}ms | rtt ${hudState.rtt}ms | jitter ${hudState.jit}ms | jbuf ${hudState.jbuf}ms | loss ${hudState.loss}% | rx ${hudState.mbps}Mbps\n` +
    `${clientBuild} | ${fullFrameDiagnostic ? "full-frame" : "sharp-zoom"} | media g${hudState.mediaGeneration} | view ${viewportTelemetry.state} r${viewportTelemetry.revision} ${viewportTelemetry.crop}\n` +
    qualityHudText();
  if (diagnosticsOutput && diagnosticsVisible) diagnosticsOutput.textContent = diagnosticsReport();
  renderConnectionStatus();
}, 250);

// ---- signaling --------------------------------------------------------------
const proto = location.protocol === "https:" ? "wss:" : "ws:";
let ws = null;
// Relay credentials belong only to the current signaling session. Never expose
// the ticket through URLs, diagnostics, settings or debug snapshots.
let signalingPc = null;
let signalingTicket = null;
// Set once by startSession (dk-app.js). The private key is a non-extractable CryptoKey.
let sessionIdentity = null;
let sessionPc = null;
let sessionPinnedPublic = null;
let sessionGate = null;
let sessionTouched = false;
// Clock time of the last input send; the log uploader stays quiet for 250 ms after it.
let lastInputSentMs = -Infinity;
let authenticatedSession = null;
let pendingRtcTracks = [];
let signalingGeneration = 0;
let reconnectTimer = null;
let terminalSignalingFailure = false;
// Signaling liveness (performance.now ms): the oldest unanswered ping, and the last inbound message.
let signalingPingSentMs = null;
let signalingInboundMs = 0;
let pc = null;
let lastIceCandidateError = null;
let dc = null, motionDc = null;
let inputReady = false;
// Set while a monitor transition gates input with held keys/buttons kept.
let inputTransitionHold = false;
let closingPeer = false;
let cursorReconciler = null;
const latchedModifiers = new Set();
const input = new InputController({
  sendControl: data => { lastInputSentMs = Date.now(); return sendInput(data, dc); },
  sendMotion: data => { lastInputSentMs = Date.now(); return sendInput(data, motionDc); },
  motionBuffered: () => motionDc?.bufferedAmount || (usingWssMedia ? ws?.bufferedAmount || 0 : 0),
  controlState: inputControlState,
});
// Control input (keys, buttons, clipboard) is queued while the lane is
// congested and discarded only when it is gated or dead; the controller then
// sends release-all before the next event.
function inputControlState() {
  if (!authenticatedSession?.mediaReady) return "down";
  if (desktopInputState !== "enabled" || !desktopAvailable) return "down";
  if (desktopAwaitingFrame) return inputTransitionHold && (dc?.readyState === "open" || (usingWssMedia && ws?.readyState === WebSocket.OPEN)) ? "gated" : "down";
  if (usingWssMedia && ws?.readyState === WebSocket.OPEN) return ws.bufferedAmount > 16_384 ? "busy" : "open";
  if (authenticatedSession.rtcReady && dc?.readyState === "open") return dc.bufferedAmount > 262_144 ? "busy" : "open";
  return "down";
}
function sendInput(data, channel) {
  if (!authenticatedSession?.mediaReady) return false;
  if (desktopInputState !== "enabled") return false;
  if (!desktopAvailable || desktopAwaitingFrame) return false;
  if (!usingWssMedia && authenticatedSession.rtcReady && channel?.readyState === "open") try { channel.send(data); return true; } catch {}
  // The WSS media fallback deliberately keeps input/cursor on /ws.
  if (usingWssMedia && signalingRegistered && ws?.readyState === WebSocket.OPEN && ws.bufferedAmount <= 16_384) return authenticatedSession.sendInput(data);
  return false;
}
let motionFrame = null;
// A refused motion tail (congestion) stays pending in the controller; retry on the
// next frame while input is ready, so wheel distance is delivered rather than dropped.
function armMotionFlush() { if (motionFrame === null) motionFrame = requestAnimationFrame(() => { motionFrame = null; input.flushMotion(); hudState.motionSent = input.stats.sent; hudState.motionCoalesced = input.stats.coalesced; hudState.motionDropped = input.stats.dropped; retryMotionTail(); }); }
function retryMotionTail() { if (inputReady && input.hasPendingMotion()) armMotionFlush(); }
function queueMotion(dx, dy, wheelX = 0, wheelY = 0) { input.motion(dx, dy, wheelX, wheelY); armMotionFlush(); }
function updateMotionHud() { hudState.motionSent = input.stats.sent; hudState.motionCoalesced = input.stats.coalesced; hudState.motionDropped = input.stats.dropped; }
function queueTouchMotion(dx, dy, wheelX = 0, wheelY = 0) { input.motionNow(dx, dy, wheelX, wheelY); retryMotionTail(); updateMotionHud(); }
// Reconcile against the cursor sequence, not the last wheel: wheel-only sends never move the pointer.
function queueTouchAbsolute(x, y) { const before = input.lastCursorSequence; input.absolute(x, y); const sent = input.flushMotion(); if (input.lastCursorSequence !== before) cursorReconciler?.noteSent(input.lastCursorSequence); retryMotionTail(); updateMotionHud(); return sent; }
function queueAbsolute(x, y) { input.absolute(x, y); armMotionFlush(); }
function releaseInput(send = true) { inputTransitionHold = false; input.releaseAll(send); held.clear(); latchedModifiers.clear(); buttons = touchButtons = 0; if (motionFrame !== null) cancelAnimationFrame(motionFrame); motionFrame = null; renderKeyboardModifiers?.(); }
let connected = false;
let offerSent = false;
let peerGeneration = 0;
let pendingRemoteIce = [];
let remoteDescriptionSet = false;
let currentIceServers = STUN_FALLBACK;
let mediaEpoch = null;
let usingWssMedia = false;
// WSS video decode+draw in a Worker (wss-worker.js). Null on the main-thread path.
// While set, the canvases belong to the worker: sizes come from its messages.
let wssWorker = null;
let wssWorkerInit = Promise.resolve(false);
let wssWorkerDisabled = false; // a worker failure this page: stay on the main thread
let wssCanvasDims = null;
let wssWorkerHybrid = null;
let wssTileCapability = null;
let wssCaptureId = 0;
const wssCaptureWaiters = new Map();
let wssSelected = requestedTransport === TRANSPORT.WSS || startOnRememberedWss;
let wssReconnectTimer = null;
let wssKeyframeTimer = null;
let wssDecoderEpoch = 0;
let wssNoOutputFailures = 0;
let wssRecoveryPending = false;
let wssRecoveryState = "awaiting-keyframe";
let wssLastRecoveryAt = -Infinity;
let wssStartupRecoveries = 0;
// WSS v4 single-socket session state.
const wssV4 = {
  gate: new V4ReceiveGate(), socket: null, socketOpen: false, everOpened: false, hostPresent: false,
  reconnecting: false, backoffMs: 0, reconnects: 0, heartbeatTimer: null, lastSendAt: 0, lastInboundAt: 0,
  lastFrameAt: null, lastSeq: 0, decodedSeq: undefined, lastPresentAt: -Infinity,
  presentSkips: 0, gateDrops: 0, frames: 0,
};
function wssV4Status() {
  const open = wssV4.socketOpen && wssV4.hostPresent;
  return wssV4StatusLabel({
    socketOpen: open,
    reconnecting: wssV4.reconnecting || (wssV4.socketOpen && !wssV4.hostPresent && hasPresentedFrame),
    framesPresented: hasPresentedFrame ? 1 : 0,
    lastFrameAtMs: wssV4.lastFrameAt,
    nowMs: performance.now(),
  });
}
let wssStartedHost = false;
let wssStartInFlight = false;
let wssPresentedSequence = 0;
let debugFirstMediaPacket = false;
let debugFirstDecoderConfigured = false;
let debugFirstDecoderOutput = false;
let wssMediaReceivedBytes = 0;
let wssMediaReceivedPackets = 0;
let wssPacketRejects = { total: 0, byReason: {} };
let wssDecodeSequenceByTimestamp = new Map();
let wssFrameMeta = new WeakMap();
let wssLatestFrameTrace = null;
let wssPresentedFrameTraces = new Map();
let wssTimestampFallbackLogged = false;
const hybridTileCache = new HybridTileCache();
const hevcTileCache = new HevcTileCache();
let rtcStartupFallback = null;
let rtcOfferSignaled = false;
let signalingRegistered = false;
let signalingRegisteredAtMs = null;
let signalingHostEvidence = { seen: false, type: null, atMs: null };
let lastSignalingError = null;
let lastSignalingRecycle = null;

function isCurrentPeer(generation, candidate = pc) {
  return generation === peerGeneration && candidate === pc;
}

// Relay-level messages only: hello, register-proof and the keepalive ping.
function sendRelay(message) {
  if (ws?.readyState !== WebSocket.OPEN) return false;
  ws.send(JSON.stringify(message));
  return true;
}

// Every endpoint JSON message on /ws is encrypted; relay messages use sendRelay instead.
function sendControl(message) {
  if (!signalingRegistered || ws?.readyState !== WebSocket.OPEN) return false;
  return authenticatedSession?.sendControl(message) || false;
}

function recordHostEvidence(type) {
  signalingHostEvidence = { seen: true, type, atMs: Date.now() };
}

function recordSignalingError(kind, message, detail = null) {
  lastSignalingError = { kind, message: String(message || kind), detail, atMs: Date.now() };
}

// Viewer events go to the local browser log (dk-log.js). Phase B uploads them to the PC.
function viewerLog(level, event, detail = {}) {
  globalThis.BPLog?.log(level, "receiver", event, { attempt: diagnosticAttemptId, build: clientBuild,
    transport: requestedTransport, codec: codecSession.codec, ...detail });
}

function reconnectSignaling(reason) {
  if (terminalSignalingFailure) return;
  if (reconnectTimer || ws?.readyState === WebSocket.OPEN || ws?.readyState === WebSocket.CONNECTING) return;
  hudState.ws = `reconnecting (${reason})`;
  const connect = () => {
    reconnectTimer = null;
    latestInputStatus = null;
    inputStatusReason = null;
    authenticatedSession?.close();
    authenticatedSession = null;
    const generation = ++signalingGeneration;
    const socketUrl = signalingSocketUrl();
    debugTrace("signaling-construct", {
      path: new URL(socketUrl).pathname,
      generation,
      reason,
    });
    const socket = new WebSocket(socketUrl);
    socket.binaryType = "arraybuffer";
    ws = socket;
    let receiveQueue = Promise.resolve();
    socket.onopen = () => {
      if (ws !== socket || generation !== signalingGeneration) return;
      hudState.ws = "open";
      debugTrace("signaling-open", { generation });
      sendRelay(viewerHello({ identity: sessionIdentity, turn: wantsTurn(params) }));
    };
    const handleClose = event => {
      if (ws !== socket || generation !== signalingGeneration) return;
      const admitted = signalingRegistered;
      hudState.ws = "closed";
      latestInputStatus = null;
      inputStatusReason = null;
      signalingRegistered = false;
      signalingPc = null;
      signalingTicket = null;
      authenticatedSession?.close();
      authenticatedSession = null;
      cleanupPeer();
      recordSignalingError("socket-close", `WebSocket closed (${event.code})`, { code: event.code, reason: event.reason || "" });
      debugTrace("signaling-close", { generation, code: event.code, reason: event.reason || "" });
      viewerLog(event.code === 1008 ? "warn" : "info", "signaling-close", { generation, closeCode: event.code, reason: event.reason || "", admitted });
      hud.classList.add("bad");
      if (event.code === 1008) {
        // Policy close: refused before hello-ok, or dropped from the PC's list. Never reconnect.
        terminalSignalingFailure = true;
        releaseInput();
        if (!admitted) showNotAdmitted(event.reason);
        return;
      }
      if (!shouldReconnectAfterClose(event.code, terminalSignalingFailure)) {
        terminalSignalingFailure = true;
        return;
      }
      restartPeer("ws-closed", false);
      reconnectSignaling("ws-closed");
    };
    socket.onerror = () => { if (ws === socket) { hudState.ws = "error"; recordSignalingError("socket-error", "WebSocket signaling error"); debugTrace("signaling-error", { generation }); } };
    // WebCrypto yields between messages. Preserve wire order across hello-ok,
    // auth-host and ctl rather than letting WebSocket callbacks race each other.
    socket.onmessage = ev => {
      if (ws === socket) signalingInboundMs = performance.now();
      receiveQueue = receiveQueue.then(() => handleSignal(socket, generation, ev)).catch(error => {
        if (ws === socket && generation === signalingGeneration) viewerLog("error", "signaling-message-failed", { error: String(error?.message || error), stack: String(error?.stack || "") });
        if (ws === socket && generation === signalingGeneration) endAuthenticatedSession({ title: "Session ended", body: "A control message failed its integrity check, so the session was closed." });
      });
    };
    socket.onclose = event => {
      // A final encrypted refusal precedes close on the wire, but decryption
      // may still be running. Let that refusal decide whether to reconnect.
      receiveQueue = receiveQueue.then(() => handleClose(event));
    };
  };
  if (reason === "startup") connect();
  else reconnectTimer = setTimeout(connect, 250);
}

function signalingSocketUrl() {
  return `${proto}//${location.host}/ws?pc=${encodeURIComponent(sessionPc)}`;
}

function recycleSignaling(reason) {
  if (terminalSignalingFailure) return;
  latestInputStatus = null;
  inputStatusReason = null;
  invalidatePendingMonitorRequest(reason);
  const renderedAgeMs = lastRenderedDueMs === null ? null : performance.now() - lastRenderedDueMs;
  const diagnostic = createSignalingRecycleDiagnostic({
    reason,
    signalingGeneration,
    signalingRegistered,
    socketReadyState: ws?.readyState ?? null,
    usingWssMedia,
    mediaSocketOpen: wssV4.socketOpen,
    mediaGeneration: Number.isSafeInteger(hudState.mediaGeneration) ? hudState.mediaGeneration : null,
    reconstructed: wssV4.lastSeq,
    presented: wssPresentedSequence,
    receivedPackets: wssMediaReceivedPackets,
    recoveryState: wssRecoveryState,
    renderedAgeMs,
    online: navigator.onLine,
  });
  lastSignalingRecycle = { ...diagnostic, atMs: Date.now() };
  debugTrace("signaling-recycle", diagnostic);
  viewerLog("warn", "signaling-recycle", diagnostic);
  // Invalidate first so the intentional close cannot restart a peer with
  // credentials from the old signaling epoch.
  const socket = ws;
  ws = null;
  signalingRegistered = false;
  signalingPc = null;
  signalingTicket = null;
  authenticatedSession?.close();
  authenticatedSession = null;
  signalingGeneration++;
  try { socket?.close(1000, reason); } catch {}
  reconnectSignaling(reason);
}

async function handleSignal(socket, generation, ev) {
  if (ws !== socket || generation !== signalingGeneration) return;
  // The relay never forwards plaintext endpoint frames after cutover.
  if (typeof ev.data !== "string") { console.info("BubblePlay ignored a plaintext endpoint frame."); return; }
  let msg;
  try { msg = JSON.parse(ev.data); } catch { return; }
  if (!msg || typeof msg !== "object") return;
  switch (msg.type) {
    case "register-challenge": {
      if (signalingRegistered || !sessionIdentity) return;
      const proof = await registerProof({ identity: sessionIdentity, origin: relayOrigin(location.href), nonce: msg.nonce });
      if (!proof || ws !== socket || generation !== signalingGeneration) return;
      sendRelay(proof);
      break;
    }
    case "hello-ok": {
      if (signalingRegistered) return;
      signalingPc = typeof msg.pc === "string" && msg.pc ? msg.pc : null;
      signalingTicket = typeof msg.ticket === "string" && msg.ticket ? msg.ticket : null;
      if (signalingPc !== sessionPc || !signalingTicket) return endAuthenticatedSession({ title: "PC not reachable", body: "This PC does not support device keys or is not reachable." });
      signalingRegistered = true;
      signalingRegisteredAtMs = Date.now();
      signalingPingSentMs = null;
      debugTrace("signaling-hello-ok", { generation, hasMediaEpoch: typeof msg.mediaEpoch === "string" });
      hudState.ws = "ok";
      currentIceServers = validateIceServers(msg.iceServers);
      mediaEpoch = typeof msg.mediaEpoch === "string" && /^[\x20-\x7e]{1,64}$/.test(msg.mediaEpoch) ? msg.mediaEpoch : null;
      renderAbout({ relay: typeof msg.workerVersionId === "string" ? msg.workerVersionId.slice(0, 96) : null });
      // File transfer hook (2/5): file sockets pair on the epoch.
      noteFileEpoch(mediaEpoch);
      const current = () => ws === socket && generation === signalingGeneration;
      let logUploader = null;
      const session = createViewerSession({ identity: sessionIdentity, pcId: sessionPc, pinnedPublic: sessionPinnedPublic,
        relayOrigin: relayOrigin(location.href), epoch: msg.mediaEpoch, transport: wssSelected ? "wss" : "webrtc",
        send: message => current() && sendRelay(message),
        onAuthenticated: () => {
          if (!current()) return;
          viewerLog("info", "signaling-authenticated");
          // Logs and saved reports go to the PC when it offers client-logs-v1. Never blocks the session.
          try {
            if (session.logsNegotiated && globalThis.BPLog && !logUploader) {
              logUploader = createLogUploader({ log: globalThis.BPLog, pc: sessionPc,
                send: message => current() && session.sendControl(message),
                bufferedAmount: () => ws?.bufferedAmount || 0, lastInputAt: () => lastInputSentMs,
                isActive: () => current() && authenticatedSession === session && ws?.readyState === WebSocket.OPEN });
              logUploader.start();
            }
          } catch (error) { try { globalThis.BPLog?.warn("receiver", "log upload did not start", { cause: String(error?.message ?? error).slice(0, 160) }); } catch {} }
          sendStreamMode("signaling-start");
          if (wssSelected) requestWssMediaStart();
          else startPeer(peerGeneration);
        },
        onMessage: message => { if (current()) return handleEndpointSignal(socket, generation, message); },
        onLogMessage: message => { if (current()) logUploader?.handle(message); },
        onBinary: (kind, bytes) => { if (current() && kind === "cursor") handleCursorState(bytes, "signaling"); },
        onMediaReady: () => { if (current()) openAuthenticatedMedia(); },
        onFailure: (refusal, reason) => { if (current()) { viewerLog("warn", "session-refused", { reason: String(reason || "unknown").slice(0, 80), title: refusal?.title }); endAuthenticatedSession(refusal); } },
      });
      authenticatedSession = session;
      await session.start();
      break;
    }
    case "auth-host":
    case "auth-bind":
    case "ctl":
      await authenticatedSession?.receive(msg);
      break;
    case "peer-left":
      recycleSignaling("peer-left");
      restartPeer("peer-left", false);
      break;
    case "error":
      recordSignalingError("relay-error", msg.message, msg);
      viewerLog("error", "relay-error", { message: typeof msg.message === "string" ? msg.message.slice(0, 200) : "" });
      endAuthenticatedSession({ title: "Session ended", body: typeof msg.message === "string" ? msg.message.slice(0, 200) : "The relay ended this session." });
      break;
    case "ping":
    case "pong":
    case "peer-joined":
    case "pair-closed":
      break;
    default:
      console.info("BubblePlay ignored a plaintext endpoint message.");
  }
}

async function handleEndpointSignal(socket, generation, msg) {
  if (ws !== socket || generation !== signalingGeneration || !authenticatedSession?.controlReady) return;
  if (["answer", "ice", "quality", "bench-status", "media-view", "desktop-state", "input-status", "host-capabilities", "first-frame", "monitor-list", "monitor-error"].includes(msg.type)) recordHostEvidence(msg.type);
  switch (msg.type) {
    case "answer": {
      const candidate = pc;
      const peer = peerGeneration;
      if (!candidate || remoteDescriptionSet) return;
      try {
        await candidate.setRemoteDescription({ type: "answer", sdp: msg.sdp });
        if (!isCurrentPeer(peer, candidate)) return;
        remoteDescriptionSet = true;
        maybeBindRtcPeer(candidate, peer);
        rtcStartupFallback?.progress();
        hudState.ice = "answer";
        const queued = pendingRemoteIce;
        pendingRemoteIce = [];
        for (const ice of queued) await addRemoteIce(candidate, peer, ice);
      } catch (e) {
        if (isCurrentPeer(peer, candidate)) hudState.ice = `err:${e}`;
      }
      break;
    }
    case "ice":
      if (msg.candidate) {
        if (!pc || !remoteDescriptionSet) pendingRemoteIce.push(msg.candidate);
        else addRemoteIce(pc, peerGeneration, msg.candidate);
      }
      break;
    case "quality": {
      const quality = parseHostQuality(msg);
      if (quality) hostQuality = quality;
      break;
    }
    case "bench-status":
      applyBenchStatus(msg);
      break;
    case "media-view":
      handleMediaView(msg);
      break;
    case "desktop-state":
      handleDesktopState(msg);
      break;
    case "secure-action-result":
      if (msg.action === "deny") {
        secureDenyState = { pending: false, error: msg.ok ? "" : (typeof msg.reason === "string" && msg.reason ? msg.reason : "failed") };
        renderDesktopState();
      }
      break;
    case "input-status": {
      if (Object.hasOwn(msg, "reason") && ![null, "admin-required", "admin-unavailable"].includes(msg.reason)) break;
      if (Object.hasOwn(msg, "route") && !["local", "admin", "secure"].includes(msg.route)) break;
      const previousFailures = latestInputStatus?.failedPackets || 0;
      // Foreground warning updates carry empty receipt counters. Preserve the
      // receipt diagnostics and failure baseline until a receipt arrives.
      const warningOnly = Object.hasOwn(msg, "reason") && ["receivedPackets", "acceptedPackets",
        "failedPackets", "droppedPackets", "releasedInputs", "requestedEvents", "appliedEvents"]
        .every(field => msg[field] === 0);
      if (!warningOnly) latestInputStatus = msg;
      // Receipt-only updates omit reason; only an explicit null clears it.
      if (Object.hasOwn(msg, "reason")) inputStatusReason = msg.reason;
      renderConnectionStatus();
      if ((msg.failedPackets || 0) > previousFailures) viewerLog("error", "host-input-application-failed", {
        generation: msg.generation, requestedEvents: msg.requestedEvents,
        appliedEvents: msg.appliedEvents, failedPackets: msg.failedPackets,
        lastErrorCode: msg.lastErrorCode ?? null,
      });
      break;
    }
    case "input-capabilities":
      applyInputCapabilities(msg);
      break;
    case "clipboard-image":
    case "clipboard":
      receiveRemoteClipboard(msg);
      break;
    case "host-capabilities":
      applyInputCapabilities(null);
      hostCapabilities = {
        serviceMode: msg.serviceMode === true,
        coldBoot: msg.coldBoot === true,
        secureDesktop: msg.secureDesktop === true,
        secureText: msg.secureText === true,
      };
      hostCapabilitiesGeneration = generation;
      // File transfer hook (3/5): all file UI stays hidden without the capability.
      setFileTransferAvailable(Number.isInteger(msg.fileTransfer) && msg.fileTransfer >= 1);
      nativeFileDropAvailable = Number.isInteger(msg.fileDrop) && msg.fileDrop >= 1;
      nativeFileDropPreviewAvailable = msg.fileDropPreview === 1 && nativeFileDropAvailable && fileUi.enabled;
      if (!nativeFileDropPreviewAvailable) fileDragPreview.cancel();
      renderFilesSlot();
      // Follow active window: show the switch and resend a saved "on" per connect.
      applyFollowCapability(msg);
      // Local PC sound: the strip button shows only when the host streams audio.
      hostAudioCapable = Number.isInteger(msg.audio) && msg.audio >= 1;
      renderSoundSlot();
      if (wssSelected && !usingWssMedia) requestWssMediaStart();
      break;
    case "file-offer":
      fileClient.setOffer(msg);
      break;
    case "first-frame":
      handleFirstFrameMessage(msg);
      break;
    case "monitor-list":
      handleMonitorList(msg);
      break;
    case "monitor-error":
      handleMonitorError(msg);
      break;
    case "error":
      recordSignalingError("endpoint-error", msg.message, msg);
      viewerLog("error", "signaling-error", { message: msg.message });
      terminalSignalingFailure = true;
      hudState.ws = `err:${msg.message}`;
      hud.classList.add("bad");
      endAuthenticatedSession({ title: "Session ended", body: typeof msg.message === "string" ? msg.message.slice(0, 200) : "The PC ended this session." });
      break;
  }
}

// The host's trickled candidates are bare SDP strings with no mid. Bind each
// to the first m-line of the remote description (the host bundles everything
// there); index 0 is the fallback. Without a mid or index the browser throws.
function remoteIceInit(ice, remoteSdp) {
  const section = (remoteSdp || "").split(/^m=/m)[1] || "";
  const mid = /^a=mid:(\S+)/m.exec(section)?.[1];
  return mid ? { candidate: ice, sdpMid: mid, sdpMLineIndex: 0 } : { candidate: ice, sdpMLineIndex: 0 };
}

async function addRemoteIce(candidate, generation, ice) {
  try {
    await candidate.addIceCandidate(remoteIceInit(ice, candidate.remoteDescription?.sdp));
  } catch (e) {
    if (isCurrentPeer(generation, candidate)) console.warn("addIceCandidate", e);
  }
}

// Keep the relay's WebSocket alive (15 s idle heartbeat, same as the host), and
// recycle it silently when the relay stops answering (recovery.js signalingUnanswered).
setInterval(() => {
  if (!signalingRegistered) return;
  const now = performance.now();
  if (signalingUnanswered({ pingSentMs: signalingPingSentMs, inboundMs: signalingInboundMs, nowMs: now })) {
    signalingPingSentMs = null;
    recycleSignaling("signaling-silent");
    return;
  }
  if (signalingPingSentMs === null || signalingInboundMs >= signalingPingSentMs) signalingPingSentMs = now;
  sendRelay({ type: "ping" });
}, V4_HEARTBEAT_MS);

// ---- WebRTC ----------------------------------------------------------------
function endAuthenticatedSession(refusal) {
  terminalSignalingFailure = true;
  authenticatedSession?.close();
  authenticatedSession = null;
  clearTimeout(reconnectTimer); reconnectTimer = null;
  const socket = ws;
  ws = null;
  signalingGeneration++;
  signalingRegistered = false;
  signalingPc = signalingTicket = null;
  clearFirstFrameWatchdog();
  cleanupPeer();
  noteFileEpoch(null);
  setFileTransferAvailable(false);
  hudState.ws = "error";
  hud.classList.add("bad");
  try { socket?.close(1000, "session-ended"); } catch {}
  sessionGate?.showRefusal({ ...refusal, actionLabel: "Back to PCs", onAction: () => location.assign(pcListUrl(location.href)) });
}

function updateAuthenticatedInputReady() {
  const wasReady = inputReady;
  inputReady = !!authenticatedSession?.mediaReady && (usingWssMedia || (authenticatedSession.rtcReady && dc?.readyState === "open" && motionDc?.readyState === "open"));
  hudState.control = usingWssMedia ? (inputReady ? "open" : "closed") : dc?.readyState || "closed";
  hudState.motion = usingWssMedia ? (inputReady ? "open" : "closed") : motionDc?.readyState || "closed";
  hudState.dc = inputReady ? "open" : "closed";
  if (!wasReady && inputReady) onInputReady();
}

function openAuthenticatedMedia() {
  if (!authenticatedSession?.mediaReady) return;
  sessionGate?.hide();
  if (!sessionTouched && sessionPc) { sessionTouched = true; touchPc(sessionPc).catch(() => {}); }
  if (!wssSelected && !usingWssMedia) {
    const tracks = pendingRtcTracks;
    pendingRtcTracks = [];
    for (const { event, generation } of tracks) {
      if (!isCurrentPeer(generation) || event.track.readyState === "ended") continue;
      event.track.enabled = true;
      onTrack(event, generation);
    }
  }
  updateAuthenticatedInputReady();
}

function maybeBindRtcPeer(candidate = pc, generation = peerGeneration) {
  if (!candidate || !isCurrentPeer(generation, candidate) || usingWssMedia || wssSelected ||
      candidate.connectionState !== "connected" || !candidate.localDescription?.sdp || !candidate.remoteDescription?.sdp) return;
  void authenticatedSession?.bindPeer({ localSdp: candidate.localDescription.sdp, remoteSdp: candidate.remoteDescription.sdp });
}

function restartPeer(reason, reconnect = true) {
  const generation = ++peerGeneration;
  invalidatePendingMonitorRequest(reason);
  clearFirstFrameWatchdog();
  firstFrameHandshake.requestId = null;
  firstFrameHandshake.phase = "idle";
  firstFrameHandshake.admitted = false;
  firstFrameHandshake.presented = false;
  firstFrameHandshake.rtpGate = null;
  transitionNeedsKeyframe = false;
  transitionGatePassedToken = null;
  firstFrameHandshake.failureReason = "";
  desktopAwaitingFrame = true;
  renderDesktopState();
  hostQuality = null;
  cleanupPeer();
  pendingRemoteIce = [];
  remoteDescriptionSet = false;
  offerSent = false;
  rtcOfferSignaled = false;
  hudState.ice = reason;
  hud.classList.add("bad");
  if (ws?.readyState === WebSocket.OPEN) {
    if (wssSelected) requestWssMediaStart();
    else startPeer(generation);
  }
  else if (reconnect) reconnectSignaling(reason);
}

function cleanupPeer() {
  for (const { event } of pendingRtcTracks) event.track.stop();
  pendingRtcTracks = [];
  fileDragPreview.cancel();
  nativeFileDropPreviewAvailable = false;
  fileDropPresentedGeneration = null;
  fileDropPresentedViewportRevision = null;
  closingPeer = true;
  releaseInput();
  if (usingWssMedia) stopWssMedia();
  soundController.detachSink("rtc");
  clearWssTimers();
  authenticatedSession?.resetPeer();
  resetKeyboardDraft();
  resetTouchCursor();
  cursorAppearance.reset();
  inputReady = false;
  connected = false;
  try { dc?.close(); } catch {}
  try { motionDc?.close(); } catch {}
  dc = null;
  motionDc = null;
  try { pc?.close(); } catch {}
  pc = null;
  closingPeer = false;
  try { decoder?.close(); } catch {}
  decoder = null;
  if (encodedWorker) {
    encodedWorker.terminate();
    encodedWorker = null;
    replaceTransferredCanvas();
  }
  activeVideo?.remove();
  activeVideo = null;
  nativePresentation = null;
  previousInboundStats = null;
  hudState.route = "…";
  stage.classList.remove("native-video-input");
  resetRenderPacing();
  resetPresentationCounters();
}

function clearWssTimers() {
  rtcStartupFallback?.stop(); rtcStartupFallback = null;
  clearWssKeyframeWatchdog();
  clearInterval(wssV4.heartbeatTimer); wssV4.heartbeatTimer = null;
  clearTimeout(wssReconnectTimer); wssReconnectTimer = null;
}

function stopWssMedia() {
  usingWssMedia = false;
  teardownWssWorker();
  clearWssTimers();
  stopWssAudio();
  wssDecoderEpoch++;
  try { wssV4.socket?.close(1000, "switch"); } catch {}
  Object.assign(wssV4, { socket: null, socketOpen: false, everOpened: false, hostPresent: false, reconnecting: false,
    backoffMs: 0, lastFrameAt: null, lastSeq: 0, decodedSeq: undefined, lastPresentAt: -Infinity });
  wssV4.gate.reset("start");
  wssStartedHost = false;
  wssPresentedSequence = 0; wssDecodeSequenceByTimestamp.clear(); wssFrameMeta = new WeakMap();
  debugFirstMediaPacket = false;
  debugFirstDecoderConfigured = false; debugFirstDecoderOutput = false;
  wssPresentedFrameTraces.clear();
  wssLatestFrameTrace = null; wssTimestampFallbackLogged = false;
  clearWssHybridCaches();
  requestWssKeyframe.lastAt = 0;
  wssRecoveryPending = false;
  wssRecoveryState = "awaiting-keyframe";
  wssLastRecoveryAt = -Infinity;
  viewportState = { enabled: false, revision: viewportRevision, generation: 0 };
  viewportPending = null;
  viewportDesired = null;
  viewportLast = null;
  clearTimeout(viewportTimer); viewportTimer = null;
  clearTimeout(viewportTransitionTimer); viewportTransitionTimer = null;
  viewportConsecutiveTimeouts = 0;
  sharpDisabledForSession = false;
  renderedSharpView = null;
  sharpScreen.classList.remove("active");
}

function sendWssV4(text) {
  const socket = wssV4.socket;
  if (!socket || socket.readyState !== WebSocket.OPEN) return false;
  try { socket.send(text); } catch { return false; }
  wssV4.lastSendAt = performance.now();
  return true;
}

function scheduleWssV4Reconnect(reason) {
  clearTimeout(wssReconnectTimer);
  wssV4.backoffMs = nextBackoffMsV4(wssV4.backoffMs);
  wssV4.reconnecting = true;
  debugTrace("media-v4-reconnect-scheduled", { reason, delayMs: wssV4.backoffMs });
  wssReconnectTimer = setTimeout(() => {
    wssReconnectTimer = null;
    if (!usingWssMedia) return;
    wssV4.reconnects++;
    openWssV4Socket();
  }, wssV4.backoffMs);
}

function openWssV4Socket() {
  if (!usingWssMedia || !mediaEpoch || !signalingTicket || !authenticatedSession?.mediaReady) return;
  const ticket = signalingTicket;
  const socket = new WebSocket(mediaSocketUrl());
  socket.binaryType = "arraybuffer";
  wssV4.socket = socket;
  wssV4.socketOpen = false;
  wssV4.hostPresent = false;
  const current = () => usingWssMedia && wssV4.socket === socket && signalingTicket === ticket && authenticatedSession?.mediaReady;
  socket.onopen = () => {
    if (!current()) return;
    socket.send(JSON.stringify({ type: "hello", role: "media-viewer", ticket }));
    wssV4.lastSendAt = wssV4.lastInboundAt = performance.now();
  };
  socket.onmessage = (event) => {
    if (!current()) return;
    wssV4.lastInboundAt = performance.now();
    if (typeof event.data !== "string") {
      onWssV4Packet(event.data);
      return;
    }
    let message;
    try { message = JSON.parse(event.data); } catch { return; }
    if (message.type === "hello-ok") {
      const reconnect = wssV4.everOpened;
      wssV4.socketOpen = true;
      wssV4.everOpened = true;
      wssV4.reconnecting = false;
      wssV4.backoffMs = 0;
      // Whatever was in flight on the old socket is gone: the decoder must
      // restart from the keyframe the host sends when it sees this peer.
      wssV4.gate.reset(reconnect ? "reconnect" : "connect");
      debugTrace("media-v4-open", { reconnect, reconnects: wssV4.reconnects });
      if (reconnect) viewerLog("info", "wss-v4-reconnected", { reconnects: wssV4.reconnects });
      maybeStartWssHost();
    } else if (message.type === "media-peer") {
      const present = message.present === true;
      if (present && !wssV4.hostPresent) wssV4.gate.reset("host-joined");
      wssV4.hostPresent = present;
    }
  };
  socket.onclose = (event) => {
    if (!current()) return;
    wssV4.socketOpen = false;
    wssV4.hostPresent = false;
    wssV4.socket = null;
    stopWssAudio();
    hudState.ice = "wss v4 reconnecting";
    debugTrace("media-v4-close", { code: event.code, reason: event.reason || "", wasClean: event.wasClean });
    scheduleWssV4Reconnect("socket-close");
  };
  socket.onerror = () => {
    if (!current()) return;
    try { socket.close(); } catch {}
  };
}

function onWssV4Packet(data) {
  // BPA1 audio shares the media socket. It skips the v4 counters, ACK and gate.
  if (isAudioPacket(data)) {
    onWssAudioPacket(data);
    return;
  }
  wssMediaReceivedBytes += data.byteLength;
  wssMediaReceivedPackets++;
  const frame = parseFrameV4(data);
  if (!frame) {
    recordWssPacketRejection("v4-invalid-packet");
    return;
  }
  const nowMs = performance.now();
  // ACK on receipt: the host measures delivery delay, not decode time.
  sendWssV4(ackMessageV4({ generation: frame.generation, seq: frame.seq, recvTsMs: nowMs,
    decodedSeq: wssV4.decodedSeq }));
  wssV4.frames++;
  wssV4.lastFrameAt = nowMs;
  wssV4.lastSeq = frame.seq;
  if (!debugFirstMediaPacket) {
    debugFirstMediaPacket = true;
    debugTrace("media-first-packet", { bytes: data.byteLength, protocol: WSS_PROTOCOL });
  }
  const verdict = wssV4.gate.accept(frame);
  if (verdict.requestKeyframe) requestWssKeyframe(verdict.requestKeyframe);
  if (!verdict.decode) {
    wssV4.gateDrops++;
    hudState.dropped++;
    return;
  }
  if (frame.key) hudState.keyframes++;
  decodeWssFrame({
    codec: frame.codec,
    generation: frame.generation,
    sequence: frame.seq,
    timestampUs: Number(frame.captureTsUs % 9_007_199_254_740_991n),
    key: frame.key,
    data: frame.data,
    frameTrace: { sequence: frame.seq, receivedAtMs: nowMs, refine: frame.refine },
  });
}

function onWssAudioPacket(data) {
  wssAudioPlayer ??= createWssAudioPlayer();
  if (wssAudioPlayer.push(data) === "rejected") {
    recordWssPacketRejection("audio-invalid-packet");
    return;
  }
  if (!wssAudioSinkAttached) {
    wssAudioSinkAttached = true;
    const player = wssAudioPlayer;
    soundController.attachSink("wss", {
      start: () => player.resume(),
      setMuted: muted => player.setMuted(muted),
      stop: () => player.stop(),
    });
  }
}

function createWssAudioPlayer() {
  return new WssAudioPlayer({
    createContext: () => new AudioContext({ latencyHint: "interactive", sampleRate: 48000 }),
    createDecoder: init => new AudioDecoder(init),
    createChunk: init => new EncodedAudioChunk(init),
    onError: error => viewerLog("warn", "wss-audio-error", { name: error?.name || "error" }),
  });
}

function stopWssAudio() {
  if (wssAudioPlayer) wssAudioRetired = sumAudioStats(wssAudioRetired, wssAudioPlayer.stats());
  wssAudioPlayer?.stop();
  wssAudioPlayer = null;
  if (wssAudioSinkAttached) {
    wssAudioSinkAttached = false;
    soundController.detachSink("wss");
  }
}

// Diagnostics: "off" without the host capability, otherwise the controller state.
function audioSnapshot() {
  return {
    announced: hostAudioCapable,
    controller: hostAudioCapable ? soundController.state : "off",
    wss: sumAudioStats(wssAudioRetired, wssAudioPlayer?.stats()),
    webrtc: latestInboundAudio ? { ...latestInboundAudio } : null,
  };
}

function startWssV4() {
  clearInterval(wssV4.heartbeatTimer);
  wssV4.heartbeatTimer = setInterval(() => {
    if (!usingWssMedia) return;
    const now = performance.now();
    if (wssV4.socketOpen && now - wssV4.lastSendAt >= V4_HEARTBEAT_MS) sendWssV4('{"type":"ping"}');
    // The relay answers every heartbeat; silence means a half-open socket.
    if (wssV4.socketOpen && now - wssV4.lastInboundAt > V4_HEARTBEAT_MS * 2 + 10_000) {
      viewerLog("warn", "wss-v4-silent-socket", { silentMs: Math.round(now - wssV4.lastInboundAt) });
      try { wssV4.socket?.close(4000, "silent"); } catch {}
    }
  }, 1_000);
  openWssV4Socket();
}

function mediaSocketUrl() {
  const query = new URLSearchParams({ epoch: mediaEpoch });
  query.set("pc", signalingPc);
  return `${proto}//${location.host}/media-ws?${query}`;
}

function recordWssPacketRejection(reason) {
  wssPacketRejects.total++;
  wssPacketRejects.byReason[reason] = (wssPacketRejects.byReason[reason] || 0) + 1;
  if (wssPacketRejects.total <= 4) {
    viewerLog("error", "wss-media-packet-rejected", {
      reason,
      count: wssPacketRejects.byReason[reason],
    });
  }
}

function selectedWssDecoderPath() {
  return selectWssDecoderPath({
    requested: params.get("decoderPath"),
    supported: wssWorkerSupported(),
    disabled: wssWorkerDisabled,
  });
}

// Hand both canvases to the WSS worker. Until it answers, maybeStartWssHost
// waits, so no media arrives before the path is settled; if the worker cannot
// run, the canvases come back and decode stays on the main thread.
function beginWssWorker() {
  const client = createWssWorkerClient({
    onMessage: onWssWorkerMessage,
    onFatal: (reason) => failWssWorker(reason),
  });
  wssWorker = client;
  wssCanvasDims = { width: 0, height: 0 };
  wssWorkerHybrid = null;
  wssWorkerInit = client.start({
    canvases: { main: screen, sharp: sharpScreen },
    epoch: wssDecoderEpoch,
    codec: codecSession.codec,
  }).then((result) => {
    if (wssWorker !== client) return false; // stopped while starting
    if (result.ok) {
      // A resync during the handshake moved the epoch the worker was started with.
      client.post({ type: "resync", epoch: wssDecoderEpoch, reason: "worker-start" });
      hudState.dec = `${codecSession.codec} wss (worker)`;
      debugTrace("wss-worker-ready", {});
      return true;
    }
    viewerLog("warn", "wss-worker-unavailable", { reason: result.reason });
    teardownWssWorker();
    ensureMainCanvas();
    return false;
  });
}

function teardownWssWorker() {
  const client = wssWorker;
  if (!client) return;
  wssWorker = null;
  wssCanvasDims = null;
  wssWorkerHybrid = null;
  const transferred = client.transferred;
  client.stop();
  for (const waiter of wssCaptureWaiters.values()) waiter(null);
  wssCaptureWaiters.clear();
  if (transferred) {
    replaceTransferredCanvas();
    replaceTransferredSharpCanvas();
    renderedSharpView = null;
  }
}

function failWssWorker(reason) {
  viewerLog("error", "wss-worker-failure", { reason });
  wssWorkerDisabled = true;
  hudState.dec = `worker:${reason}`;
  stopWssMedia();
  setTimeout(requestWssMediaStart, 0);
}

function requestWssMediaStart() {
  if (usingWssMedia) return;
  if (!authenticatedSession?.controlReady || !signalingTicket) return;
  if (!mediaEpoch) { hudState.ice = "wss session unavailable"; return; }
  // Automatic fallback keeps the peer and data channels alive, but the
  // WebRTC transform worker may still own the visible canvas. Release only
  // that video sink before WSS reclaims a main-thread 2D context.
  releaseWebRtcVideoSink();
  if (selectedWssDecoderPath() === "worker") beginWssWorker(); else ensureMainCanvas();
  stage.classList.remove("native-video-input");
  wssSelected = true;
  usingWssMedia = true;
  wssPacketRejects = { total: 0, byReason: {} };
  wssRecoveryPending = true;
  wssRecoveryState = "awaiting-keyframe";
  // A new WSS epoch is a fresh desktop-state conversation.  Do not carry a
  // secure-desktop overlay from an older, disconnected session into this one;
  // wait for the first authenticated keyframe instead.
  desktopAvailable = true;
  desktopAwaitingFrame = true;
  renderDesktopState();
  authenticatedSession.useWss();
  openAuthenticatedMedia();
  // In auto mode keep the peer object alive as a control/lifecycle anchor.
  // Closing it here makes the host interpret the transport fallback as the
  // browser leaving. The host stops admitting RTP once WSS is selected.
  hudState.ice = "wss"; hudState.route = "wss"; hudState.dec = `${codecSession.codec} wss`;
  viewerLog("info", "wss-start", { epoch: mediaEpoch, protocol: WSS_PROTOCOL, rememberedRtcFailure: startOnRememberedWss });
  connected = true; updateAuthenticatedInputReady();
  wssStartedHost = false;
  // v4 decodes every frame in order: there is no deadline expiry.
  startWssV4();
}

function activeWssMain10() {
  return latestBenchStatus?.effective?.encoder?.main10 === true;
}

function activeWssHybridMode() {
  return codecSession.codec === "hevc" && [
    "png-static-roi", "png-quiescent-detail", "hevc-quiescent-tiles",
  ].includes(activeWssHybridModeName());
}

function activeWssHybridModeName() {
  return latestBenchStatus?.effective?.hybridMode || "off";
}

function activeWssHybridCache() {
  return latestBenchStatus?.effective?.hybridMode === "hevc-quiescent-tiles"
    ? hevcTileCache
    : hybridTileCache;
}

function clearWssHybridCaches() {
  hybridTileCache.clear();
  hevcTileCache.clear();
  wssWorker?.post({ type: "clear-caches" });
  wssWorkerHybrid = null;
}

// In worker mode the tile caches live in the worker; this is its last report.
function wssHybridSnapshot() {
  return wssWorker ? (wssWorkerHybrid ?? {}) : activeWssHybridCache().snapshot();
}

function activeWssHevcCodec() {
  return activeWssMain10() ? "hev1.2.4.L120.B0" : "hev1.1.6.L120.B0";
}

function applyBenchStatus(status) {
  const previousMain10 = activeWssMain10();
  const previousHybrid = activeWssHybridModeName();
  latestBenchStatus = status;
  const main10 = activeWssMain10();
  const hybrid = activeWssHybridModeName();
  if ((previousMain10 !== main10 || previousHybrid !== hybrid) &&
      usingWssMedia && codecSession.codec === "hevc") {
    wssRecoveryPending = true;
    wssRecoveryState = "awaiting-keyframe";
    clearWssHybridCaches();
    resyncWssDecoder(previousMain10 !== main10 ? "hevc-profile-change" : "hybrid-mode-change");
  }
}

async function selectWssCodec() {
  if (codecSession.codec !== "hevc" || typeof VideoDecoder === "undefined" || typeof VideoDecoder.isConfigSupported !== "function") {
    if (codecSession.codec === "hevc" && typeof VideoDecoder === "undefined") codecSession.fallbackToH264();
    return codecSession.codec;
  }
  const main10 = activeWssMain10();
  const codec = activeWssHevcCodec();
  try {
    const support = await VideoDecoder.isConfigSupported({
      codec, codedWidth: 1920, codedHeight: 1080,
      optimizeForLatency: true, hardwareAcceleration: "prefer-hardware",
    });
    if (!support?.supported) {
      if (main10) {
        viewerLog("error", "main10-configure-unsupported", { codec });
        hudState.dec = "Main10 unsupported";
        return null;
      }
      codecSession.fallbackToH264();
    }
  } catch {
    if (main10) {
      viewerLog("error", "main10-configure-unsupported", { codec });
      hudState.dec = "Main10 unsupported";
      return null;
    }
    codecSession.fallbackToH264();
  }
  if (codecSession.codec === "hevc") {
    const tileCapability = await probeHevcTileDecoderSupport(
      config => VideoDecoder.isConfigSupported(config),
    );
    hevcTileCache.setDecoderCapability(tileCapability);
    wssTileCapability = tileCapability;
    viewerLog(tileCapability.state === "supported" ? "info" : "warn",
      "hevc-tile-decoder-capability", tileCapability);
  }
  return codecSession.codec;
}

async function maybeStartWssHost() {
  hudState.ice = wssV4.socketOpen ? "wss" : "wss connecting";
  if (wssStartedHost || wssStartInFlight || !wssV4.socketOpen) return;
  wssStartInFlight = true;
  const [codec, audioDecodable] = await Promise.all([selectWssCodec(), wssAudioDecodable, wssWorkerInit]);
  if (!codec) {
    wssStartInFlight = false;
    return;
  }
  if (!usingWssMedia || wssStartedHost || !wssV4.socketOpen) {
    wssStartInFlight = false;
    return;
  }
  // selectWssCodec may have fallen back to H.264 after the worker started.
  wssWorker?.post({ type: "set-codec", codec: codecSession.codec });
  if (wssTileCapability) wssWorker?.post({ type: "tile-capability", capability: wssTileCapability });
  wssStartedHost = true;
  wssStartInFlight = false;
  sendControl({ type: "media-control", action: "start", protocol: WSS_PROTOCOL, epoch: mediaEpoch,
    codec, streamMode: streamModeValue(), reason: "client-start",
    quality: qualityPreset === "auto" ? "high" : qualityPreset, ...(audioDecodable ? { audio: 1 } : {}) });
  startFirstFrameHandshake("wss-host-start");
  scheduleSharpViewport(fullFrameDiagnostic ? "diagnostic-full-frame" : "wss-start");
}

function requestWssKeyframe(reason, requestId = null) {
  const now = performance.now();
  if (!requestId && requestWssKeyframe.lastAt && now - requestWssKeyframe.lastAt < 250) return false;
  requestWssKeyframe.lastAt = now;
  viewerLog("warn", "wss-keyframe-request", { reason, requestId });
  if (usingWssMedia && !requestId) return sendWssV4(keyframeMessageV4(reason));
  sendControl({ type: "media-control", action: "keyframe", protocol: WSS_PROTOCOL, epoch: mediaEpoch, reason,
    ...(requestId ? { requestId } : {}), codec: codecSession.codec });
  return true;
}

function clearWssKeyframeWatchdog() {
  clearTimeout(wssKeyframeTimer);
  wssKeyframeTimer = null;
}

function armWssKeyframeWatchdog() {
  clearWssKeyframeWatchdog();
  const epoch = wssDecoderEpoch;
  const timer = setTimeout(() => {
    if (wssKeyframeTimer !== timer || epoch !== wssDecoderEpoch) return;
    wssKeyframeTimer = null;
    noteWssFailure("no-output-after-keyframe");
  }, 2_000);
  wssKeyframeTimer = timer;
}

function resyncWssDecoder(reason) {
  hudState.resyncs++;
  clearWssKeyframeWatchdog();
  wssDecoderEpoch++;
  wssWorker?.post({ type: "resync", epoch: wssDecoderEpoch, reason });
  try { decoder?.close(); } catch {} decoder = null;
  latencyGuard.reset(reason);
  resetRenderPacing();
  wssDecodeSequenceByTimestamp.clear(); wssFrameMeta = new WeakMap();
  wssPresentedFrameTraces.clear();
  viewerLog("warn", "wss-recovery-reset", {
    reason,
    decoderEpoch: wssDecoderEpoch,
    hybrid: wssHybridSnapshot(),
  });
  clearWssHybridCaches();
  wssV4.gate.invalidate(reason);
  hudState.dec = `waiting keyframe (${reason})`;
}

function beginWssRecovery(reason, requestId = null) {
  const now = performance.now();
  const probeInterval = streamModeValue() === "game" ? 1_000 : 2_000;
  const identifiedRequest = typeof requestId === "string" && requestId.length > 0;
  if (wssRecoveryState !== "healthy") {
    if (identifiedRequest ||
        (wssRecoveryState === "awaiting-keyframe" && now - wssLastRecoveryAt >= probeInterval)) {
      wssLastRecoveryAt = now;
      const requested = requestWssKeyframe(reason, requestId);
      if (requested) armWssKeyframeWatchdog();
      return requested;
    }
    return false;
  }
  if (!identifiedRequest && now - wssLastRecoveryAt < probeInterval) return false;
  wssRecoveryPending = true;
  wssRecoveryState = "awaiting-keyframe";
  wssLastRecoveryAt = now;
  resyncWssDecoder(reason);
  const requested = requestWssKeyframe(reason, requestId);
  if (requested) armWssKeyframeWatchdog();
  return requested;
}

function handleRtcPeerConnectionState(generation, candidate, state) {
  if (!isCurrentPeer(generation, candidate)) return;
  rtcStartupFallback?.progress();
  if (state === "connected" && !usingWssMedia) {
    maybeBindRtcPeer(candidate, generation);
    startFirstFrameHandshake("peer-connected");
  }
  const action = rtcPeerEndAction({ usingWssMedia, connectionState: state });
  if (action === "end-peer") endRtcPeerOnly(`rtc-${state}`);
  else if (action === "recycle") {
    // A terminal failure is evidence of an unusable RTC path. Auto mode must
    // retain WSS rescue without treating a still-connecting peer as failed.
    if (requestedTransport === TRANSPORT.AUTO) {
      if (!wssSelected) switchToWss("rtc-failed");
      return;
    }
    hudState.ice = state;
    hud.classList.add("bad");
    if (ws?.readyState === WebSocket.OPEN) ws.close(1000, "rtc-failed");
    else restartPeer(state);
  }
}

// WSS carries this session: close only the RTC peer and its data channels.
// Input already falls back to the signaling socket when the lanes are gone.
function endRtcPeerOnly(reason) {
  if (!pc) return;
  soundController.detachSink("rtc");
  closingPeer = true;
  try { dc?.close(); } catch {}
  try { motionDc?.close(); } catch {}
  dc = null;
  motionDc = null;
  try { pc.close(); } catch {}
  pc = null;
  closingPeer = false;
  rtcStartupFallback?.stop(); rtcStartupFallback = null;
  debugTrace("rtc-peer-ended", { reason });
  viewerLog("warn", "rtc-peer-ended", { reason, transport: "wss" });
}

function switchToWss(reason) {
  if (usingWssMedia) return;
  // Auto mode only reaches here when WebRTC could not start in this browser.
  rememberRtcFailure(rtcFailureStorage());
  viewerLog("warn", "rtc-failed-switch-to-wss", { reason, remembered: true });
  wssSelected = true;
  hudState.ice = `wss (${reason})`;
  requestWssMediaStart();
}

let lastNetworkRecoveryAt = -Infinity;
let networkRouteProbeTimer = null;
const NETWORK_ROUTE_PROBE_MS = 1_500;
function networkRouteHealthSnapshot() {
  const renderedAgeMs = lastRenderedDueMs === null ? null : performance.now() - lastRenderedDueMs;
  return {
    signalingGeneration,
    online: navigator.onLine,
    signalingRegistered,
    signalingSocketOpen: ws?.readyState === WebSocket.OPEN,
    usingWssMedia,
    mediaSocketOpen: wssV4.socketOpen,
    recoveryState: wssRecoveryState,
    // A static v4 desktop sends no frames by design; heartbeats cover liveness.
    allowQuiescentIdle: true,
    receivedPackets: wssMediaReceivedPackets,
    reconstructed: wssV4.lastSeq,
    presented: wssPresentedSequence,
    renderedAgeMs,
    peerConnectionState: pc?.connectionState ?? null,
  };
}

function probeNetworkRouteChange() {
  if (terminalSignalingFailure || !navigator.onLine || networkRouteProbeTimer !== null) return;
  const before = networkRouteHealthSnapshot();
  viewerLog("info", "network-route-probe", { phase: "start", ...before });
  networkRouteProbeTimer = setTimeout(() => {
    networkRouteProbeTimer = null;
    if (terminalSignalingFailure || before.signalingGeneration !== signalingGeneration) return;
    const after = networkRouteHealthSnapshot();
    const maxRenderedAgeMs = 1_000;
    const recover = shouldRecoverAfterNetworkRouteProbe(before, after, { maxRenderedAgeMs });
    viewerLog(recover ? "warn" : "info", "network-route-probe", {
      phase: "complete", recover, maxRenderedAgeMs, ...after,
    });
    if (recover) recoverNetworkRoute("connection-change-stalled");
  }, NETWORK_ROUTE_PROBE_MS);
}

function recoverNetworkRoute(reason) {
  if (terminalSignalingFailure || !navigator.onLine) return;
  const now = performance.now();
  if (now - lastNetworkRecoveryAt < 5_000) return;
  lastNetworkRecoveryAt = now;
  hudState.ice = `route change (${reason})`;
  // A Wi-Fi/cellular handoff can leave apparently-open TCP/WebRTC sockets on
  // the old route. Rebuild the signaling epoch and all selected media lanes;
  // hello-ok will restore WSS or start a fresh WebRTC offer.
  recycleSignaling(`network-${reason}`);
  restartPeer(`network-${reason}`, false);
}

function recoverWssStartup(reason) {
  if (!usingWssMedia) return;
  viewerLog("error", "wss-startup-failure", { reason, attempt: wssStartupRecoveries + 1 });
  if (requestedTransport === TRANSPORT.WSS) {
    // Forced WSS remains forced: rebuild the epoch, but never silently switch
    // to a different media transport.
    recoverNetworkRoute(`wss-${reason}`);
    return;
  }
  if (wssStartupRecoveries === 0) {
    wssStartupRecoveries = 1;
    recoverNetworkRoute(`wss-retry-${reason}`);
    return;
  }
  wssSelected = false;
  stopWssMedia();
  viewerLog("warn", "wss-fallback-webrtc", { reason });
  recycleSignaling(`wss-fallback-${reason}`);
  restartPeer(`wss-fallback-${reason}`, false);
}

addEventListener("online", () => recoverNetworkRoute("online"));
const routeConnection = navigator.connection || navigator.mozConnection || navigator.webkitConnection;
routeConnection?.addEventListener("change", probeNetworkRouteChange);

function noteWssFailure(reason) {
  viewerLog("error", "wss-failure", { reason });
  const hardCodecFailure = reason === "webcodecs-unsupported" || reason === "configure-unsupported" ||
    reason === "decode-error" || reason === "decode-throw";
  const main10 = activeWssMain10();
  if (reason === "no-output-after-keyframe") wssNoOutputFailures++;
  const exhaustedHevcRecovery = reason === "no-output-after-keyframe" && wssNoOutputFailures >= 3;
  if (!main10 && codecSession.codec === "hevc" && (hardCodecFailure || exhaustedHevcRecovery) && codecSession.fallbackToH264()) {
    wssNoOutputFailures = 0;
    sendControl({ type: "media-control", action: "keyframe", reason, codec: codecSession.codec });
    hudState.dec = "H264 fallback";
    stopWssMedia();
    setTimeout(requestWssMediaStart, 0);
    return;
  }
  beginWssRecovery(reason);
  hudState.dec = `waiting keyframe (${reason})`;
}

// Worker path: the worker does the codec/hybrid checks, decode and draw. This
// only forwards the frame with the main-thread state it needs (hybrid mode,
// geometry for the frame's generation, desktop availability); the bytes move,
// they are not copied. The epoch lets the worker drop frames sent before a resync.
function postWssFrameToWorker(frame) {
  rtcStartupFallback?.stop();
  hudState.mediaGeneration = frame.generation;
  const bytes = frame.data;
  wssWorker.post({
    type: "frame",
    epoch: wssDecoderEpoch,
    codec: frame.codec,
    generation: frame.generation,
    sequence: frame.sequence,
    key: frame.key,
    timestampUs: frame.timestampUs,
    buffer: bytes.buffer,
    byteOffset: bytes.byteOffset,
    byteLength: bytes.byteLength,
    hevcCodec: activeWssHevcCodec(),
    hybridMode: activeWssHybridModeName(),
    desktopAvailable,
    view: viewportForGeneration(frame.generation),
    frameTrace: frame.frameTrace,
  }, [bytes.buffer]);
}

function decodeWssFrame(frame) {
  if (wssWorker) {
    if (wssWorker.ready) postWssFrameToWorker(frame);
    return;
  }
  if (typeof VideoDecoder === "undefined") { noteWssFailure("webcodecs-unsupported"); return; }
  const detectedCodec = detectAnnexBCodec(frame.data, { key: frame.key });
  const headerCodec = frame.codec === CODEC.HEVC ? "hevc" : "h264";
  if (!frame.key && (detectedCodec || headerCodec) !== codecSession.codec) return;
  const prepared = prepareHybridWssFrame(frame, {
    active: activeWssHybridMode() && detectedCodec !== "h264",
    cache: activeWssHybridCache(),
  });
  if (!prepared) {
    noteWssFailure("hybrid-frame-invalid");
    return;
  }
  const bitstreamCodec = detectedCodec || detectAnnexBCodec(prepared.data, { key: frame.key });
  const codec = bitstreamCodec || headerCodec;
  if (codec !== codecSession.codec) {
    if (!frame.key || !bitstreamCodec) return;
    followDecoderCodec(codec);
  }
  if (prepared.activity && (prepared.activity.upserts || prepared.activity.invalidations ||
      prepared.activity.clears || prepared.activity.pngBytes)) {
    viewerLog("info", "hybrid-frame-activity", {
      generation: frame.generation,
      sequence: frame.sequence,
      cropRevision: prepared.cropRevision,
      ...prepared.activity,
      hybrid: activeWssHybridCache().snapshot(),
    });
  }
  const timestamp = Number.isFinite(frame.timestampUs)
    ? frame.timestampUs : Math.round(frame.timestamp90k * 1_000_000 / 90_000);
  hudState.mediaGeneration = frame.generation;
  if (frame.key && wssRecoveryState === "awaiting-keyframe") {
    wssRecoveryPending = true;
    wssRecoveryState = "awaiting-keyframe-output";
    viewerLog("info", "wss-recovery-keyframe", { generation: frame.generation, sequence: frame.sequence, codec: frame.codec });
  }
  const frameTrace = frame.frameTrace ? { ...frame.frameTrace } : {};
  if (prepared.activity) {
    frameTrace.hybridCropRevision = prepared.cropRevision;
    frameTrace.hybridTileUpserts = prepared.activity.upserts;
    frameTrace.hybridTileInvalidations = prepared.activity.invalidations;
    frameTrace.hybridTileSuppressions = prepared.activity.suppressions;
    frameTrace.hybridTileReactivations = prepared.activity.reactivations;
    frameTrace.hybridTilePermanentInvalidations = prepared.activity.permanentInvalidations;
    frameTrace.hybridTileClears = prepared.activity.clears;
    frameTrace.hybridPngBytes = prepared.activity.pngBytes;
  }
  wssDecodeSequenceByTimestamp.set(timestamp, {
    sequence: frame.sequence,
    generation: frame.generation,
    key: frame.key,
    cropRevision: prepared.cropRevision,
    frameTrace,
  });
  while (wssDecodeSequenceByTimestamp.size > 64) wssDecodeSequenceByTimestamp.delete(wssDecodeSequenceByTimestamp.keys().next().value);
  const chunk = {
    type: frame.key ? "key" : "delta",
    timestamp,
    data: prepared.data,
    codec: codecSession.codec,
    frameTrace,
  };
  feedDecoder(chunk, latencyGuard.generation);
  // Arm only after feedDecoder has created/retained a configured decoder.
  // A backpressure reset invalidates the decoder epoch and must not leave an
  // obsolete timer behind that can force a healthy HEVC session to H.264.
  if (frame.key && decoder?.state === "configured") armWssKeyframeWatchdog();
}

async function startPeer(generation = peerGeneration) {
  if (requestedTransport === TRANSPORT.WSS || usingWssMedia) return;
  if (!authenticatedSession?.controlReady) return;
  if (offerSent || generation !== peerGeneration || ws?.readyState !== WebSocket.OPEN) return;
  offerSent = true;
  lastIceCandidateError = null;

  const peerOptions = {
    iceServers: currentIceServers,
    iceTransportPolicy,
  };
  // This must be set during construction, but only the opt-in WebCodecs path
  // needs it. The native path keeps browser decode/render untouched.
  if (useWebCodecs) peerOptions.encodedInsertableStreams = true;
  let candidate;
  try {
    candidate = new RTCPeerConnection(peerOptions);
  } catch {
    // The validator should prevent this, but retain a safe direct fallback
    // if a browser rejects a server-specific ICE URI we accepted.
    if (currentIceServers === STUN_FALLBACK) {
      offerSent = false;
      hudState.ice = "ice-config-error";
      return;
    }
    currentIceServers = STUN_FALLBACK;
    try {
      candidate = new RTCPeerConnection({ ...peerOptions, iceServers: STUN_FALLBACK });
    } catch {
      offerSent = false;
      hudState.ice = "ice-config-error";
      return;
    }
  }
  pc = candidate;
  rtcStartupFallback?.stop();
  rtcOfferSignaled = false;
  if (requestedTransport === TRANSPORT.AUTO) {
    rtcStartupFallback = createRtcStartupFallback({
      isCurrent: () => isCurrentPeer(generation, candidate) && !usingWssMedia && !wssSelected,
      snapshot: () => ({
        connected: ["connected", "completed"].includes(candidate.iceConnectionState),
        peerConnected: candidate.connectionState === "connected",
        framesReceived,
        nativeFramesPresented: nativePresentation?.generation === generation ? nativePresentation.framesPresented : 0,
      }),
      getStats: () => candidate.getStats(),
      onFallback: () => switchToWss("no-rtp"),
      onHealthy: () => forgetRtcFailure(rtcFailureStorage()),
    });
  }

  // Receive-only video; the browser offers, the host answers (locked).
  const transceiver = candidate.addTransceiver("video", { direction: "recvonly" });
  // PC sound: receive-only Opus after the video m-line. Audio is not gated on video.
  candidate.addTransceiver("audio", { direction: "recvonly" });
  transceiver.receiver.playoutDelayHint = 0; // minimal jitter buffer
  // playoutDelayHint is advisory and Chromium may still grow hundreds of
  // milliseconds of buffer. jitterBufferTarget is the explicit millisecond
  // target in current Chromium; zero chooses latency over concealment.
  if ("jitterBufferTarget" in transceiver.receiver) {
    try { transceiver.receiver.jitterBufferTarget = 0; } catch { /* older implementation */ }
  }

  // Prefer HEVC for a fresh page, with H.264 after it so an H.264-only host can
  // still answer. The preference and capability/decode fallback are page-local;
  // no selection is persisted between sessions.
  try {
    const caps = RTCRtpReceiver.getCapabilities("video");
    const codec = orderRtpCodecPreferences(caps?.codecs, codecSession.codec);
    const primary = rtpCodecMimeTypes(codecSession.codec);
    const primaryAvailable = codec.some((c) => primary.includes(c.mimeType.toLowerCase()));
    if (codec.length && primaryAvailable) {
      transceiver.setCodecPreferences(codec);
    } else if (codecSession.codec === "hevc" && codecSession.fallbackToH264()) {
      offerSent = false;
      try { candidate.close(); } catch {}
      startPeer(generation);
      return;
    }
  } catch (e) {
    console.warn("setCodecPreferences", e);
  }

  // Reliable ordered state lane plus lossy latest-only motion lane.
  const channel = candidate.createDataChannel("input-control", { ordered: true });
  const motionChannel = candidate.createDataChannel("input-motion", { ordered: false, maxRetransmits: 0 });
  dc = channel;
  motionDc = motionChannel;
  channel.binaryType = "arraybuffer";
  motionChannel.binaryType = "arraybuffer";
  const updateInputReady = () => updateAuthenticatedInputReady();
  const laneClosed = () => {
    if (!isCurrentPeer(generation, candidate) || closingPeer) return;
    if (wssSelected || rtcPeerEndAction({ usingWssMedia, laneClosed: true }) === "end-peer") return endRtcPeerOnly("input-lane-closed");
    releaseInput(false); recycleSignaling("input-lane-closed"); restartPeer("input-lane-closed", false);
  };
  channel.onopen = () => { if (isCurrentPeer(generation, candidate)) { rtcStartupFallback?.progress(); updateInputReady(); } };
  channel.onclose = laneClosed;
  motionChannel.onopen = () => { if (isCurrentPeer(generation, candidate)) { rtcStartupFallback?.progress(); updateInputReady(); } };
  motionChannel.onclose = laneClosed;
  channel.onerror = (e) => { if (isCurrentPeer(generation, candidate)) hudState.dc = `err:${e.message || ""}`; };
  channel.onmessage = (event) => { if (isCurrentPeer(generation, candidate) && !usingWssMedia && authenticatedSession?.mediaReady) handleCursorState(event.data); };
  motionChannel.onmessage = (event) => { if (isCurrentPeer(generation, candidate) && !usingWssMedia && authenticatedSession?.mediaReady) handleCursorState(event.data); };

  candidate.onicecandidate = (ev) => {
    if (isCurrentPeer(generation, candidate) && ev.candidate) {
      sendControl({ type: "ice", candidate: ev.candidate.candidate });
    }
  };
  candidate.onicecandidateerror = (ev) => {
    if (!isCurrentPeer(generation, candidate)) return;
    // Keep this compact and omit address/port so profiling never reports a
    // client's private or public IP address through the signaling relay.
    lastIceCandidateError = {
      errorCode: ev.errorCode,
      errorText: ev.errorText,
      url: ev.url,
      atMs: Date.now(),
    };
  };
  candidate.oniceconnectionstatechange = () => {
    if (!isCurrentPeer(generation, candidate)) return;
    hudState.ice = candidate.iceConnectionState;
    rtcStartupFallback?.progress();
    connected = candidate.iceConnectionState === "connected" || candidate.iceConnectionState === "completed";
    if (candidate.iceConnectionState === "failed" && requestedTransport === TRANSPORT.AUTO
        && !usingWssMedia && !wssSelected && !["new", "connecting"].includes(candidate.connectionState)) {
      switchToWss("ice-failed");
      return;
    }
    if (connected) {
      hud.classList.remove("bad");
      if (!mobilePointerDevice) requestPointerLock();
      // Usually idempotent with the offer-time request. If slow ICE outlasted
      // both watchdog intervals, renew the failed handshake once media can flow.
      startFirstFrameHandshake("ice-connected");
    }
  };
  candidate.ontrack = ev => {
    if (!isCurrentPeer(generation, candidate) || usingWssMedia || wssSelected) { ev.track.stop(); return; }
    if (authenticatedSession?.mediaReady) onTrack(ev, generation);
    else {
      ev.track.enabled = false;
      // There is one receive transceiver each for video and audio.
      if (pendingRtcTracks.length >= 2) { ev.track.stop(); return; }
      pendingRtcTracks.push({ event: ev, generation });
    }
  };
  candidate.onconnectionstatechange = () => handleRtcPeerConnectionState(generation, candidate, candidate.connectionState);

  try {
    const offer = await candidate.createOffer();
    if (!isCurrentPeer(generation, candidate)) return;
    await candidate.setLocalDescription(offer);
    if (!isCurrentPeer(generation, candidate)) return;
    if (sendControl({ type: "offer", sdp: offer.sdp })) {
      rtcOfferSignaled = true;
      startFirstFrameHandshake("offer-sent");
    }
  } catch (e) {
    if (isCurrentPeer(generation, candidate)) restartPeer(`offer:${e.message || e}`);
  }
}

/**
 * Starts the session for one PC after dk-app.js unlocked the key. identity: { id,
 * publicRaw: Uint8Array, privateKey: non-extractable CryptoKey }. pinnedPublic is the
 * stored base64url PC key. The gate hides only after endpoint authentication/binding.
 */
export function startSession({ identity, pc, pinnedPublic, gate = null }) {
  if (sessionIdentity) throw new Error("A session is already running on this page.");
  if (!identity?.id || !(identity.publicRaw instanceof Uint8Array) || !identity.privateKey || typeof pc !== "string" || !pc) {
    throw new TypeError("startSession needs an unlocked identity and a PC ID.");
  }
  const pin = b64u.decode(pinnedPublic);
  if (pin.length !== 32) throw new TypeError("startSession needs the PC's pinned public key.");
  sessionIdentity = identity;
  sessionPc = pc;
  sessionPinnedPublic = pin;
  sessionGate = gate;
  terminalSignalingFailure = false;
  globalThis.BPLog?.setContext({ device: identity.id, pc, session: diagnosticAttemptId, build: clientBuild });
  viewerLog("info", "session-start", { pcKnown: true });
  reconnectSignaling("startup");
}

/** Ends the session before Lock, Switch PC or Forget this browser leave the page. */
export function endSession() {
  releaseInput();
  if (document.pointerLockElement) document.exitPointerLock?.();
  terminalSignalingFailure = true;
  authenticatedSession?.close(); authenticatedSession = null;
  clearTimeout(reconnectTimer); reconnectTimer = null;
  cleanupPeer();
  noteFileEpoch(null);
  const socket = ws;
  ws = null;
  signalingRegistered = false;
  signalingPc = signalingTicket = null;
  signalingGeneration++;
  try { socket?.close(1000, "session-ended"); } catch {}
}

function showNotAdmitted(reason = "") {
  // The relay puts a short reason in the 1008 close. Show it when present, plain text only.
  const said = typeof reason === "string" ? reason.replace(/[^ -~]/g, "").trim().slice(0, 120) : "";
  sessionGate?.showRefusal({
    title: "Not admitted",
    body: `The relay did not let this browser through to ${sessionPc}. If it was unpaired on the PC, pair again.${said ? ` Reason: ${said}.` : ""}`,
    actionLabel: "Back to PCs",
    onAction: () => location.assign(pcListUrl(location.href)),
  });
}

// ---- WebCodecs decode --------------------------------------------------------
let decoder = null;
let encodedWorker = null;
let encodedWorkerStats = null;
let nativePresentation = null;
let activeVideo = null;
let lastRenderedTimestamp = null;
let lastRenderedDueMs = null;
const latencyGuard = createLatencyGuard({
  now: () => performance.now(),
  onReset: ({ reason }) => resyncPipeline(reason),
});
let framesDrawn = 0;
let framesReceived = 0;
let fpsWindow = 0;
let lastFrameDrawnAt = null;
const framePresentationTelemetry = new FramePresentationTelemetry();
function resetPresentationCounters() {
  framesDrawn = 0;
  framesReceived = 0;
  wssMediaReceivedBytes = 0;
  wssMediaReceivedPackets = 0;
  wssPacketRejects = { total: 0, byReason: {} };
  fpsWindow = 0;
  lastFrameDrawnAt = null;
  lastDecodedDimensions = null;
  framePresentationTelemetry.reset();
  hudState.fps = 0;
  hudState.recv = 0;
  hudState.presented = 0;
  hudState.frameGap = "…";
  hudState.maxFrameGap = 0;
  hudState.dimensions = "…";
  hudState.mediaGeneration = "…";
}
setInterval(() => {
  if (useWebCodecs || usingWssMedia) hudState.fps = fpsWindow;
  fpsWindow = 0;
}, 1000);

const PATTERN = { x: 64, y: 64, cell: 24, columns: 32, bits: 64 };
// Canvas size: the worker owns the canvas on the WSS worker path (the element
// keeps its placeholder size), so the receiver tracks the size it reports.
function screenDims() {
  return wssCanvasDims || { width: screen.width, height: screen.height };
}
function screenMedia() {
  return wssCanvasDims
    ? { getBoundingClientRect: () => screen.getBoundingClientRect(), width: wssCanvasDims.width, height: wssCanvasDims.height }
    : screen;
}

// Worker path: ask the worker to render the PNG and read the timestamp pattern.
function captureFromWorker() {
  const dims = screenDims();
  if (!usingWssMedia || !wssWorker?.ready || !dims.width || !dims.height || !wssLatestFrameTrace) return Promise.resolve(null);
  const id = ++wssCaptureId;
  return new Promise((resolve) => {
    const timer = setTimeout(() => { wssCaptureWaiters.delete(id); resolve(null); }, 5_000);
    wssCaptureWaiters.set(id, (m) => {
      clearTimeout(timer);
      if (!m || m.error) { resolve(null); return; }
      resolve({
        pngDataUrl: m.pngDataUrl,
        diagnostics: {
          glassToGlass: m.glassToGlass,
          renderedTimestamp: lastRenderedTimestamp,
          framesDrawn,
          frameTrace: { ...wssLatestFrameTrace },
          frameTraceHistory: [...wssPresentedFrameTraces.values()].map(trace => ({ ...trace })),
        },
      });
    });
    wssWorker.post({ type: "capture", id, sharp: sharpScreen.classList.contains("active") });
  });
}

function readTimestampPattern(target = screen, context = ctx) {
  if (target.width < PATTERN.x + PATTERN.cell * PATTERN.columns ||
      target.height < PATTERN.y + PATTERN.cell * 2) return null;
  try {
    const image = context.getImageData(
      PATTERN.x,
      PATTERN.y,
      PATTERN.cell * PATTERN.columns,
      PATTERN.cell * 2,
    );
    const bits = [];
    for (let index = 0; index < PATTERN.bits; index++) {
      const column = index % PATTERN.columns;
      const row = Math.floor(index / PATTERN.columns);
      const px = column * PATTERN.cell + Math.floor(PATTERN.cell / 2);
      const py = row * PATTERN.cell + Math.floor(PATTERN.cell / 2);
      const offset = (py * image.width + px) * 4;
      const brightness = image.data[offset] + image.data[offset + 1] + image.data[offset + 2];
      bits.push(brightness >= 384 ? 1 : 0);
    }
    const decoded = decodeTimestampPatternBits(bits);
    if (!decoded) return null;
    const ageMs = (((Date.now() >>> 0) - decoded.timestamp) >>> 0);
    if (ageMs > 10_000) return null;
    return { ...decoded, ageMs };
  } catch {
    return null;
  }
}

if (diagnosticsEnabled) {
  window.__bubbleplayDiagnostics = {
    requestMonitors() {
      return sendControl({ type: "media-control", action: "monitors", protocol: WSS_PROTOCOL, epoch: mediaEpoch, reason: "diagnostic-refresh" });
    },
    selectMonitor(selector) {
      if (typeof selector !== "string" || !monitorList.some(monitor => monitor.selector === selector)) return false;
      return requestMonitor(selector);
    },
    injectDecoderFault() {
      const reason = "diagnostic-injected-fault";
      if (usingWssMedia) {
        wssLastRecoveryAt = -Infinity;
        return beginWssRecovery(reason);
      }
      if (!encodedWorker) return false;
      encodedWorker.postMessage({ type: "resync", reason });
      return true;
    },
    rtcPeerState() {
      return pc ? pc.connectionState : null;
    },
    setFixedZoom(zoom) {
      const value = Number(zoom);
      if (!Number.isFinite(value) || value < 1 || value > 4 || streamModeValue() !== "work") return false;
      syncViewport();
      const center = { x: innerWidth / 2, y: innerHeight / 2 };
      viewport.setZoom(1, center);
      viewport.setZoom(value, center);
      renderViewport();
      scheduleSharpViewport("diagnostic-fixed-zoom");
      return true;
    },
    panFixedViewport(dx, dy) {
      const x = Number(dx), y = Number(dy);
      if (!Number.isFinite(x) || !Number.isFinite(y) ||
          streamModeValue() !== "work" || viewport.zoom <= 1) return false;
      panBy(x, y);
      return true;
    },
    moveCursorNormalized(x, y) {
      const nx = Number(x), ny = Number(y);
      if (!canMoveAbsoluteCursor({ pointerMode, inputReady }, nx, ny)) return false;
      updateRemoteCursor(cursor => cursor.setNormalized(nx, ny));
      const point = remoteCursor.absolutePoint();
      return queueTouchAbsolute(point.x, point.y);
    },
    setStreamMode(mode) {
      if (mode !== "work" && mode !== "game") return false;
      const target = mode === "game" ? "game" : "desktop";
      if (pointerMode === target) sendStreamMode("diagnostic-mode");
      else setPointerMode(target, true);
      return true;
    },
    forceH264Fallback() {
      if (!usingWssMedia) return false;
      if (codecSession.codec === "h264") return true;
      if (!codecSession.fallbackToH264()) return false;
      hudState.dec = "H264 diagnostic fallback";
      stopWssMedia();
      setTimeout(requestWssMediaStart, 0);
      return true;
    },
    inject({ stallMs = 0, dropEvery = 0 } = {}) {
      encodedWorker?.postMessage({ type: "fault", stallMs: Math.max(0, stallMs), dropEvery: Math.max(0, dropEvery) });
    },
    injectMainThreadStall(ms = 750) {
      const duration = Math.max(0, Number(ms) || 0);
      setTimeout(() => {
        const until = performance.now() + duration;
        while (performance.now() < until) { /* deliberate diagnostic stall */ }
      }, 0);
    },
    reset() {
      hudState.dropped = 0;
      hudState.resyncs = 0;
      hudState.keyframes = 0;
      resetPresentationCounters();
    },
    captureFrame() {
      if (wssWorker) return captureFromWorker();
      if (!usingWssMedia || !screen.width || !screen.height || !wssLatestFrameTrace) return null;
      const target = sharpScreen.classList.contains("active") ? sharpScreen : screen;
      return {
        pngDataUrl: target.toDataURL("image/png"),
        diagnostics: {
          glassToGlass: readTimestampPattern(),
          renderedTimestamp: lastRenderedTimestamp,
          framesDrawn,
          frameTrace: { ...wssLatestFrameTrace },
          frameTraceHistory: [...wssPresentedFrameTraces.values()].map(trace => ({ ...trace })),
        },
      };
    },
    snapshot() {
      const snapshotAtMs = performance.now();
      const snapshotAtEpochMs = Date.now();
      const nativeFramesPresented = nativePresentation?.framesPresented || 0;
      const renderedAgeMs = lastRenderedDueMs !== null ? performance.now() - lastRenderedDueMs :
        (nativePresentation?.lastFrameAtMs === null || nativePresentation?.lastFrameAtMs === undefined ? null : performance.now() - nativePresentation.lastFrameAtMs);
      return {
        diagnosticAttemptId,
        clock: { performanceMs: snapshotAtMs, epochMs: snapshotAtEpochMs },
        ...hudState,
        generation: latencyGuard.generation,
        renderedTimestamp: lastRenderedTimestamp ?? (nativeFramesPresented > 0 ? nativeFramesPresented : null),
        renderedAgeMs,
        framesDrawn: nativeFramesPresented || framesDrawn,
        framesReceived,
        glassToGlass: useWebCodecs ? (encodedWorkerStats?.glassToGlass || readTimestampPattern()) : null,
        worker: encodedWorkerStats,
        nativePresentation: nativePresentation ? { ...nativePresentation } : null,
        rtc: latestRtcProfile ? { ...latestRtcProfile, generation: latestRtcProfileGeneration } : null,
        hostQuality,
        hostCapabilities: {
          ...hostCapabilities,
          current: hostCapabilitiesGeneration === signalingGeneration,
          generation: hostCapabilitiesGeneration,
        },
        benchStatus: latestBenchStatus,
        frameTrace: wssLatestFrameTrace ? { ...wssLatestFrameTrace } : null,
        frameTraceHistory: [...wssPresentedFrameTraces.values()].map(trace => ({ ...trace })),
        wssMedia: {
          receivedBytes: wssMediaReceivedBytes, receivedPackets: wssMediaReceivedPackets,
          protocol: WSS_PROTOCOL,
          reconstructed: wssV4.lastSeq, presented: wssPresentedSequence,
          v4: {
            status: wssV4Status(), socketOpen: wssV4.socketOpen, hostPresent: wssV4.hostPresent,
            frames: wssV4.frames, lastSeq: wssV4.lastSeq, decodedSeq: wssV4.decodedSeq ?? null,
            gateDrops: wssV4.gateDrops, presentSkips: wssV4.presentSkips, reconnects: wssV4.reconnects,
            waitingForKey: wssV4.gate.waitingForKey,
          },
          recoveryState: wssRecoveryState,
          packetRejects: {
            total: wssPacketRejects.total,
            byReason: { ...wssPacketRejects.byReason },
          },
          presentation: framePresentationTelemetry.snapshot(),
          hybrid: wssHybridSnapshot(),
          decoderPath: wssWorker?.ready ? "worker" : "main",
        },
        audio: audioSnapshot(),
        viewport: viewportHealthSnapshot(),
        signaling: {
          registered: signalingRegistered,
          registeredAtMs: signalingRegisteredAtMs,
          hostEvidence: { ...signalingHostEvidence },
          lastError: lastSignalingError ? { ...lastSignalingError } : null,
          lastRecycle: lastSignalingRecycle ? { ...lastSignalingRecycle } : null,
          monitorRevision: monitorListRevision,
          monitorSwitchPending,
          lastMonitorRequest: lastMonitorRequest ? { ...lastMonitorRequest } : null,
          lastMonitorError: lastMonitorError ? { ...lastMonitorError } : null,
          monitors: monitorList.map(monitor => ({ ...monitor })),
        },
        cursor: cursorReconciler ? { mode: pointerMode, synced: cursorReconciler.synced, pending: cursorReconciler.pending, lastSent: cursorReconciler.lastSent, lastAck: cursorReconciler.lastAck, feedbackAt: cursorReconciler.feedbackAt } : null,
      };
    },
  };
}

// Timestamp-paced render queue. Decoded frames arrive in bursts (network
// jitter); drawing them the moment they arrive makes the picture stutter.
// Instead each frame is scheduled for its RTP-timestamp-derived due time and
// drawn on the next rAF tick at that cadence. Frames that arrive more than
// one cadence interval late are dropped — never queued into latency.
const RENDER = {
  cadenceMs: 16.67,  // rolling average frame interval
  prevTs: null,
  prevWall: 0,
  queue: [],
  rafPending: false,
};
const MAX_RENDER_QUEUE = 1;

function resetRenderPacing() {
  RENDER.cadenceMs = 16.67;
  RENDER.prevTs = null;
  RENDER.prevWall = 0;
  for (const f of RENDER.queue) { try { f.frame.close(); } catch {} }
  RENDER.queue = [];
  RENDER.rafPending = false;
  lastRenderedTimestamp = null;
  lastRenderedDueMs = null;
  hudState.queued = 0;
  hudState.lag = "…";
}

function onDecodedFrame(frame) {
  let retained = false;
  try {
  const now = performance.now();
  const ts = frame.timestamp;
  framesReceived++;
  hudState.recv = framesReceived;

    if (lastRenderedTimestamp === null && RENDER.queue.length === 0) {
      latencyGuard.reanchor(ts, now);
    }

    // Encoded-frame timestamps are microseconds per the WebCodecs contract.
    if (RENDER.prevTs !== null && ts !== RENDER.prevTs) {
      const tsDelta = ts - RENDER.prevTs;
      const wallDelta = now - RENDER.prevWall;
      const interval = tsDelta / 1000;
      if (interval >= 8 && interval <= 100) {
        RENDER.cadenceMs = RENDER.cadenceMs * 0.8 + interval * 0.2;
      }
      RENDER.prevTs = ts;
      RENDER.prevWall = now;
    }
    if (RENDER.prevTs === null) {
      RENDER.prevTs = ts;
      RENDER.prevWall = now;
    }

    // TCP/WSS has already paid its delivery delay. Present the newest decoded
    // frame immediately; timestamp pacing remains only for WebRTC burst smoothing.
    const due = usingWssMedia ? now : latencyGuard.expectedWall(ts);
    if (!Number.isFinite(due)) {
      dropAndResync("missing-media-anchor");
      return;
    }
    if (usingWssMedia) {
      if (!shouldPresentDecodedV4({ decodeQueueSize: decoder?.decodeQueueSize ?? 0,
        msSinceLastPresent: now - wssV4.lastPresentAt })) {
        wssV4.presentSkips++;
        return;
      }
      wssV4.lastPresentAt = now;
    }
    if (presentWssFrameImmediately(frame, usingWssMedia, now, drawFrame, framePresentationTelemetry)) {
      retained = true;
      return;
    }
    framePresentationTelemetry.noteRafEnqueued();
    RENDER.queue.push({ frame, due });
    retained = true;
    if (RENDER.queue.length > MAX_RENDER_QUEUE) {
      const stale = RENDER.queue.shift();
      retained = stale.frame !== frame;
      framePresentationTelemetry.noteRafCoalesced();
      try { stale.frame.close(); } catch {}
    }
    if (!RENDER.rafPending) {
      RENDER.rafPending = true;
      requestAnimationFrame(renderTick);
    }
  } catch (e) {
    hudState.dec = `render:${e.message}`;
  } finally {
    if (!retained) { try { frame.close(); } catch {} }
  }
}

function renderTick() {
  try {
    RENDER.rafPending = false;
    const now = performance.now();
    // Keep only the newest queued frame — older ones are stale the moment a
    // fresher frame arrives.
    while (RENDER.queue.length > 1) {
      framePresentationTelemetry.noteRafCoalesced();
      try { RENDER.queue.shift().frame.close(); } catch {}
    }
    const front = RENDER.queue[0];
    if (!front) return;
    const due = front.due;
    // Draw when the frame is due (or past due, i.e. the decoder is slower
    // than the stream — catch up by showing the newest frame). Only wait
    // when a frame genuinely arrives early, which is real burst smoothing.
    if (!Number.isFinite(due) || due <= now) {
      RENDER.queue.shift();
      if (now - due > 100) {
        try { front.frame.close(); } finally {
          hudState.dropped++;
          framePresentationTelemetry.noteRafLateDrop();
        }
      } else {
        drawFrame(front.frame, due);
      }
    } else {
      RENDER.rafPending = true;
      requestAnimationFrame(renderTick);
    }
    if (RENDER.queue.length && !RENDER.rafPending) {
      RENDER.rafPending = true;
      requestAnimationFrame(renderTick);
    }
  } catch (e) {
    hudState.dec = `tick:${e.message}`;
  }
}

// ---- network stats via getStats (latency diagnostics) -----------------------
let previousInboundStats = null;
let hevcDecodeFailureWindows = 0;

function fallbackWebRtcCodec(reason) {
  if (usingWssMedia || codecSession.codec !== "hevc" || !codecSession.fallbackToH264()) return false;
  hudState.dec = `H264 fallback (${reason})`;
  // The host intentionally accepts one offer per signaling epoch. Recycle
  // signaling first so the H.264-only offer cannot be mistaken for a stale
  // duplicate of the failed HEVC negotiation.
  recycleSignaling(`hevc-${reason}`);
  restartPeer(`hevc-${reason}`, false);
  return true;
}

function qosPresentationMetrics() {
  if (nativePresentation) {
    return {
      fps: nativePresentation.fps || null,
      recentGapMs: nativePresentation.recentFrameGapMs,
      width: nativePresentation.width || null,
      height: nativePresentation.height || null,
    };
  }
  if (!useWebCodecs) {
    return { fps: null, recentGapMs: null, width: null, height: null };
  }
  // WebCodecs is explicit. Its Worker reports rendered FPS; the canvas owns
  // the current decoded dimensions when native rVFC is unavailable.
  return {
    fps: encodedWorkerStats?.fps || null,
    recentGapMs: null,
    width: screen.width || null,
    height: screen.height || null,
  };
}

function sendClientQos(rx, atMs) {
  if (!connected || ws?.readyState !== WebSocket.OPEN || ws.bufferedAmount > 65_536) return;
  const payload = createQosPayload({
    sessionId: profileSessionId,
    seq: ++qosSequence,
    atMs,
    rx,
    presentation: qosPresentationMetrics(),
  });
  if (!payload) return;
  const message = JSON.stringify(payload);
  if (new TextEncoder().encode(message).byteLength <= 2_048 && ws?.readyState === WebSocket.OPEN && ws.bufferedAmount <= 65_536) {
    sendControl(payload);
  }
}

function sendClientProfile() {
  if (!profilingEnabled || ws?.readyState !== WebSocket.OPEN || ws.bufferedAmount > 65_536) return;
  const connection = navigator.connection || navigator.mozConnection || navigator.webkitConnection;
  const profile = {
    schemaVersion: 1,
    sessionId: profileSessionId,
    page: {
      visibility: document.visibilityState,
      focused: document.hasFocus(),
      decoderPath: hudState.dec,
      viewport: { width: innerWidth, height: innerHeight, dpr: devicePixelRatio },
    },
    device: {
      userAgent: navigator.userAgent,
      platform: navigator.userAgentData?.platform || navigator.platform,
      mobile: navigator.userAgentData?.mobile,
      brands: navigator.userAgentData?.brands,
      hardwareConcurrency: navigator.hardwareConcurrency,
      deviceMemoryGiB: navigator.deviceMemory,
      screen: { width: window.screen.width, height: window.screen.height },
    },
    network: connection ? {
      effectiveType: connection.effectiveType,
      downlinkMbps: connection.downlink,
      rttHintMs: connection.rtt,
      saveData: connection.saveData,
      type: connection.type,
    } : null,
    peer: {
      iceConnectionState: pc?.iceConnectionState,
      iceGatheringState: pc?.iceGatheringState,
      connectionState: pc?.connectionState,
      signalingState: pc?.signalingState,
      lastIceCandidateError,
    },
    rtc: latestRtcProfile,
    worker: encodedWorkerStats,
    nativePresentation: nativePresentation ? { ...nativePresentation } : null,
    hud: { ...hudState },
  };
  const message = {
    type: "diag",
    seq: ++profileSequence,
    atMs: Date.now(),
    profile,
  };
  if (JSON.stringify(message).length <= 16_384) sendControl(message);
}

let statsPollPending = false;
setInterval(async () => {
  if (!pc) return;
  // The signaling WebSocket remains usable even when peer-to-peer ICE does
  // not. Report that state too so cellular/NAT failures are diagnosable.
  if (!["connected", "completed"].includes(pc.iceConnectionState)) {
    sendClientProfile();
    return;
  }
  if (statsPollPending) return;
  statsPollPending = true;
  try {
    const sampledPeer = pc;
    const sampledGeneration = peerGeneration;
    const stats = await sampledPeer.getStats();
    if (pc !== sampledPeer || peerGeneration !== sampledGeneration) return;
    latestRtcProfile = summarizeRtcStats(stats);
    // Stats sampled before a peer restart describe the previous epoch; the
    // generation lets consumers reject them as decode evidence.
    latestRtcProfileGeneration = sampledGeneration;
    hudState.route = deriveCandidateRoute(latestRtcProfile);
    let inbound = null, rtt = null, inboundAudio = null;
    stats.forEach((s) => {
      if (s.type === "candidate-pair" && s.state === "succeeded" && s.nominated) {
        if (typeof s.currentRoundTripTime === "number" && s.currentRoundTripTime >= 0) {
          rtt = s.currentRoundTripTime;
        }
      } else if (s.type === "inbound-rtp" && (s.kind === "video" || s.mediaType === "video")) {
        inbound = s;
      } else if (s.type === "inbound-rtp" && (s.kind === "audio" || s.mediaType === "audio")) {
        inboundAudio = s;
      }
    });
    const atMs = Date.now();
    latestInboundAudio = inboundAudio ? {
      generation: peerGeneration,
      atMs,
      packetsReceived: inboundAudio.packetsReceived,
      packetsLost: inboundAudio.packetsLost,
      totalSamplesReceived: inboundAudio.totalSamplesReceived,
      concealedSamples: inboundAudio.concealedSamples,
      jitterBufferDelay: inboundAudio.jitterBufferDelay,
      jitterBufferEmittedCount: inboundAudio.jitterBufferEmittedCount,
    } : null;
    const currentInbound = inbound ? {
      generation: peerGeneration,
      atMs,
      rtt,
      jitter: inbound.jitter,
      bytesReceived: inbound.bytesReceived,
      packetsReceived: inbound.packetsReceived,
      packetsLost: inbound.packetsLost,
      jitterBufferDelay: inbound.jitterBufferDelay,
      jitterBufferEmittedCount: inbound.jitterBufferEmittedCount,
      nackCount: inbound.nackCount,
      pliCount: inbound.pliCount,
      framesReceived: inbound.framesReceived,
      framesDecoded: inbound.framesDecoded,
      framesDropped: inbound.framesDropped,
      freezeCount: inbound.freezeCount,
      qpSum: inbound.qpSum,
    } : { generation: peerGeneration, atMs, rtt };
    const rx = computeQosRxWindow(currentInbound, previousInboundStats);
    if (!usingWssMedia && codecSession.codec === "hevc" && inbound) {
      const mediaArrived = (inbound.framesReceived || 0) > 0 || (inbound.bytesReceived || 0) > 0;
      const decoded = (inbound.framesDecoded || 0) > 0 || (nativePresentation?.framesPresented || 0) > 0 || framesDrawn > 0;
      hevcDecodeFailureWindows = mediaArrived && !decoded ? hevcDecodeFailureWindows + 1 : 0;
      if (hevcDecodeFailureWindows >= 2) {
        hevcDecodeFailureWindows = 0;
        fallbackWebRtcCodec("decode-timeout");
        return;
      }
    }
    previousInboundStats = currentInbound;
    if (rx.rttMs !== null) hudState.rtt = rx.rttMs;
    if (rx.jitterMs !== null) hudState.jit = rx.jitterMs;
    if (rx.lossPct !== null) hudState.loss = Math.round(rx.lossPct);
    if (rx.mbps !== null) hudState.mbps = rx.mbps.toFixed(1);
    if (rx.jitterBufferMs !== null) hudState.jbuf = Math.round(rx.jitterBufferMs);
    sendClientQos(rx, atMs);
  } catch {
    // stats unavailable (not connected / mid-restart)
  } finally {
    statsPollPending = false;
    sendClientProfile();
  }
}, 1000);

// ---- WSS worker results ------------------------------------------------------
function onWssWorkerMessage(m) {
  if (!wssWorker) return;
  switch (m.type) {
    case "log": viewerLog(m.level, m.name, m.fields); return;
    case "trace": debugTrace(m.name, m.fields); return;
    case "capture-result": {
      const waiter = wssCaptureWaiters.get(m.id);
      if (waiter) { wssCaptureWaiters.delete(m.id); waiter(m); }
      return;
    }
    case "worker-error": failWssWorker(m.message || "worker-error"); return;
    default: break;
  }
  // Everything else describes decoder state: ignore what predates the last resync.
  if (m.epoch !== wssDecoderEpoch) return;
  switch (m.type) {
    case "decoder-configured": hudState.dec = `${m.codec} (worker)`; break;
    case "failure":
      if (m.hud) hudState.dec = m.hud;
      noteWssFailure(m.reason);
      break;
    case "drop":
      hudState.dropped++;
      if (m.recover) beginWssRecovery(m.recover);
      break;
    case "key-accepted":
      if (wssRecoveryState === "awaiting-keyframe") {
        wssRecoveryPending = true;
        wssRecoveryState = "awaiting-keyframe-output";
        viewerLog("info", "wss-recovery-keyframe", { generation: m.generation, sequence: m.sequence, codec: m.codec });
      }
      break;
    case "key-fed": armWssKeyframeWatchdog(); break;
    case "codec-follow": {
      if (!codecSession.followBitstreamCodec(m.codec)) break;
      clearWssKeyframeWatchdog();
      resetRenderPacing();
      wssPresentedFrameTraces.clear();
      wssNoOutputFailures = 0;
      if (wssRecoveryState === "awaiting-keyframe-output") wssRecoveryState = "awaiting-keyframe";
      viewerLog("info", "decoder-keyframe-codec-change", { previousCodec: m.previousCodec, codec: m.codec });
      break;
    }
    case "out": onWssWorkerFrameOut(m); break;
    default: break;
  }
}

// The worker decoded (and maybe drew) a frame: the receiver-side bookkeeping that
// onDecodedFrame/drawFrame do on the main-thread path.
function onWssWorkerFrameOut(m) {
  clearWssKeyframeWatchdog();
  wssNoOutputFailures = 0;
  if (m.decodedSequence !== null) wssV4.decodedSeq = m.decodedSequence;
  if (m.key && wssRecoveryState === "awaiting-keyframe-output") {
    wssRecoveryPending = false;
    wssRecoveryState = "healthy";
  }
  framesReceived++;
  hudState.recv = framesReceived;
  hudState.queued = m.queued;
  if (!m.attempted) { wssV4.presentSkips++; return; }
  framePresentationTelemetry.noteImmediateAttempt();
  if (m.hybrid) wssWorkerHybrid = m.hybrid;
  if (m.width) lastDecodedDimensions = { width: m.width, height: m.height };
  if (!m.presented) return;
  const w = m.width, h = m.height;
  if (m.sharp) {
    layoutSharpCanvas(m.view);
    const gain = cropGain(m.view, w, h);
    hudState.dimensions = `${m.view.width}x${m.view.height}→${w}x${h} ${gain.x.toFixed(1)}x/${gain.y.toFixed(1)}x`;
  } else {
    if (m.resized) {
      wssCanvasDims = { width: w, height: h };
      syncViewportAndFollow();
    }
    layoutSharpCanvas(null);
    hudState.dimensions = `${w}x${h}`;
  }
  markFirstFramePresented(m.generation ?? null, m.cropRevision ?? null,
    { gatePassed: transitionGatePassedToken !== null && transitionGatePassedToken === firstFrameHandshake.requestId });
  framePresentationTelemetry.noteImmediatePresented();
  const renderedAt = m.renderedAt;
  wssLatestFrameTrace = { ...m.trace };
  wssPresentedFrameTraces.set(m.trace.sequence >>> 0, { ...m.trace });
  while (wssPresentedFrameTraces.size > 64) wssPresentedFrameTraces.delete(wssPresentedFrameTraces.keys().next().value);
  if (lastFrameDrawnAt !== null) {
    const frameGap = Math.max(0, Math.round(renderedAt - lastFrameDrawnAt));
    hudState.frameGap = frameGap;
    hudState.maxFrameGap = Math.max(Number(hudState.maxFrameGap) || 0, frameGap);
  }
  lastFrameDrawnAt = renderedAt;
  lastRenderedTimestamp = m.timestamp;
  lastRenderedDueMs = renderedAt;
  hudState.lag = 0;
  fpsWindow++;
  framesDrawn++;
  hudState.presented = framesDrawn;
  wssPresentedSequence = m.sequence;
}

function onTrack(ev, generation = peerGeneration) {
  if (generation !== peerGeneration) return;
  if (ev.track.kind === "audio") {
    attachRtcAudio(ev.track, generation);
    return;
  }
  const receiver = ev.receiver;
  const transceiver = pc.getTransceivers().find((t) => t.receiver === receiver);

  if (!useWebCodecs) {
    startNativeVideo(ev.track, generation);
    return;
  }

  // 1) Modern path: RTCRtpScriptTransform (Worker-based, current Chrome).
  if (transceiver && "transform" in receiver && typeof RTCRtpScriptTransform !== "undefined") {
    const worker = new Worker("/latency-worker.js", { type: "module" });
    encodedWorker = worker;
    let transformAttached = false;
    const reloadFallback = () => {
      const next = new URL(location.href);
      next.searchParams.delete("decoderPath");
      location.replace(next.href);
    };
    worker.onmessage = (e) => {
      if (generation !== peerGeneration || worker !== encodedWorker) return;
      const message = e.data;
      if (message.kind === "control") {
        if (message.type === "capabilities") {
          if (!message.supported) {
            worker.terminate(); encodedWorker = null;
            if (receiver.createEncodedStreams) startEncodedStreams(receiver, generation); else startNativeVideo(ev.track, generation);
            return;
          }
          try {
            const offscreen = screen.transferControlToOffscreen();
            worker.postMessage({ type: "canvas", canvas: offscreen, diagnostics: diagnosticsEnabled }, [offscreen]);
          } catch {
            worker.terminate(); encodedWorker = null;
            if (receiver.createEncodedStreams) startEncodedStreams(receiver, generation); else startNativeVideo(ev.track, generation);
          }
          return;
        }
        if (message.type === "worker-ready" && !transformAttached) {
          // The canvas must be owned by the Worker before media starts.
          transformAttached = true;
          receiver.transform = new RTCRtpScriptTransform(worker, {});
          hudState.dec = "transform (off-main-thread)";
          return;
        }
        if (message.stats) {
          const workerPresented = Number(message.stats.framesDrawn || 0) > 0;
          encodedWorkerStats = message.stats;
          lastRenderedTimestamp = message.stats.renderedTimestamp;
          lastRenderedDueMs = message.stats.renderedAgeMs === null ? null : performance.now() - message.stats.renderedAgeMs;
          framesDrawn = message.stats.framesDrawn;
          framesReceived = message.stats.framesReceived;
          hudState.recv = message.stats.framesReceived;
          hudState.fps = message.stats.fps;
          hudState.dropped = message.stats.dropped;
          // This worker belongs to the current peerGeneration. Once it has
          // drawn a frame, presentation is concrete and cannot come from the
          // receiver epoch discarded during a privileged desktop switch.
          // During a WebRTC transition the worker reports frames drawn after
          // it passed the admitted keyframe for this request (rtpGate).
          const gatePassed = message.stats.transitionToken === firstFrameHandshake.requestId && Number(message.stats.transitionDrawn || 0) > 0;
          if (workerPresented) markFirstFramePresented(null, null, { gatePassed });
        }
        if (message.type === "transition-unprovable") {
          // The admitted keyframe arrived without an RTP timestamp to compare.
          if (message.token === firstFrameHandshake.requestId) recoverRtcInPlaceTransition("no-frame-rtp-timestamp");
        } else if (message.type === "resync") { hudState.resyncs++; hudState.dec = `waiting keyframe (${message.reason})`; }
        else if (message.type === "keyframe-requested") {
          hudState.keyframes++;
          hudState.dec = `waiting keyframe (${message.reason || "recovery"})`;
        } else if (message.type === "keyframe-request-error") {
          hudState.dec = "waiting periodic IDR (keyframe request unavailable)";
        } else if (message.type === "worker-error") {
          hudState.dec = `worker:${message.message}`;
          reloadFallback();
        } else if (message.type === "worker-init-error" || message.type === "passthrough-error") {
          hudState.dec = `${message.type}:${message.message || "unavailable"}`;
          if (!transformAttached) {
            worker.terminate(); encodedWorker = null; replaceTransferredCanvas();
            if (receiver.createEncodedStreams) startEncodedStreams(receiver, generation); else startNativeVideo(ev.track, generation);
          } else {
            reloadFallback();
          }
        }
        return;
      }
    };
    worker.onerror = (e) => { if (generation === peerGeneration) { hudState.dec = `worker:${e.message || "err"}`; reloadFallback(); } };
    worker.postMessage({ type: "capabilities" });
    return;
  }

  // 2) Legacy path: createEncodedStreams (older Chromium).
  if (receiver.createEncodedStreams) {
    startEncodedStreams(receiver, generation);
    return;
  }

  // An explicit WebCodecs request can still fall back to native video.
  startNativeVideo(ev.track, generation);
}

// PC sound over WebRTC: a hidden, unmuted element per peer. Muting is local.
function attachRtcAudio(track, generation = peerGeneration) {
  if (generation !== peerGeneration) return;
  const element = document.createElement("audio");
  element.autoplay = true;
  element.hidden = true;
  document.body.appendChild(element);
  element.srcObject = new MediaStream([track]);
  soundController.attachSink("rtc", {
    start: () => element.play().then(() => true, () => false),
    setMuted: muted => { element.muted = muted; },
    stop: () => { element.pause(); element.srcObject = null; element.remove(); },
  });
}

function startNativeVideo(track, generation = peerGeneration) {
  const video = document.createElement("video");
  video.autoplay = true;
  video.playsInline = true;
  video.muted = true;
  video.style.cssText = "position:fixed;inset:0;width:100%;height:100%;object-fit:contain;background:#000;z-index:1;pointer-events:none;";
  visual.appendChild(video);
  video.srcObject = new MediaStream([track]);
  stage.classList.add("native-video-input");
  activeVideo = video;
  nativePresentation = {
    generation,
    supported: typeof video.requestVideoFrameCallback === "function",
    firstFrameAtMs: null,
    firstFramePerformanceMs: null,
    framesPresented: 0,
    fps: 0,
    recentFrameGapMs: null,
    maxFrameGapMs: 0,
    width: 0,
    height: 0,
    mediaTime: null,
    presentationTime: null,
    expectedDisplayTime: null,
    processingDurationMs: null,
    lastFrameAtMs: null,
    rtpTimestamps: false,
    lastRtpTimestamp: null,
  };
  let lastFrameAt = null;
  let fpsWindowStartedAt = null;
  let framesInFpsWindow = 0;
  const updateDimensions = () => {
    if (generation !== peerGeneration || activeVideo !== video || !nativePresentation) return;
    nativePresentation.width = video.videoWidth;
    nativePresentation.height = video.videoHeight;
    hudState.dimensions = video.videoWidth && video.videoHeight ? `${video.videoWidth}x${video.videoHeight}` : "…";
  };
  video.addEventListener("loadedmetadata", () => { updateDimensions(); syncViewportAndFollow(); });
  // An in-place monitor switch can change the stream resolution mid-track.
  video.addEventListener("resize", () => { updateDimensions(); syncViewportAndFollow(); });
  if (nativePresentation.supported) {
    const onVideoFrame = (now, metadata) => {
      if (generation !== peerGeneration || activeVideo !== video) return;
      // A presented native frame is the concrete media arrival signal for this
      // peer epoch: cancel the RTC auto-fallback exactly like feedDecoder does
      // for the encoded path, and surface the frame in the received counter.
      rtcStartupFallback?.stop();
      // metadata.rtpTimestamp proves which host generation this frame is from.
      const rtpTimestamp = Number.isInteger(metadata.rtpTimestamp) ? metadata.rtpTimestamp >>> 0 : null;
      if (rtpTimestamp !== null) { nativePresentation.rtpTimestamps = true; nativePresentation.lastRtpTimestamp = rtpTimestamp; }
      markFirstFramePresented(null, null, { rtpTimestamp });
      const gap = lastFrameAt === null ? null : now - lastFrameAt;
      if (nativePresentation.firstFrameAtMs === null) nativePresentation.firstFrameAtMs = Date.now();
      if (nativePresentation.firstFramePerformanceMs === null) nativePresentation.firstFramePerformanceMs = now;
      nativePresentation.framesPresented = metadata.presentedFrames ?? nativePresentation.framesPresented + 1;
      nativePresentation.recentFrameGapMs = gap === null ? null : Math.round(gap);
      nativePresentation.maxFrameGapMs = gap === null ? 0 : Math.max(nativePresentation.maxFrameGapMs, Math.round(gap));
      nativePresentation.mediaTime = metadata.mediaTime ?? null;
      nativePresentation.presentationTime = metadata.presentationTime ?? null;
      nativePresentation.expectedDisplayTime = metadata.expectedDisplayTime ?? null;
      nativePresentation.processingDurationMs = typeof metadata.processingDuration === "number" ? Math.round(metadata.processingDuration * 1000) : null;
      nativePresentation.lastFrameAtMs = now;
      updateDimensions();
      if (fpsWindowStartedAt === null) fpsWindowStartedAt = now;
      framesInFpsWindow++;
      if (now - fpsWindowStartedAt >= 1_000) {
        nativePresentation.fps = Math.round(framesInFpsWindow * 1_000 / (now - fpsWindowStartedAt));
        framesInFpsWindow = 0;
        fpsWindowStartedAt = now;
      }
      hudState.fps = nativePresentation.fps;
      hudState.presented = nativePresentation.framesPresented;
      hudState.frameGap = nativePresentation.recentFrameGapMs ?? "…";
      hudState.maxFrameGap = nativePresentation.maxFrameGapMs || "…";
      framesDrawn = nativePresentation.framesPresented;
      if (nativePresentation.framesPresented > framesReceived) {
        framesReceived = nativePresentation.framesPresented;
      }
      hudState.recv = framesReceived;
      lastFrameAt = now;
      video.requestVideoFrameCallback(onVideoFrame);
    };
    video.requestVideoFrameCallback(onVideoFrame);
  }
  video.addEventListener("loadeddata", () => { if (video.videoWidth && video.videoHeight) markFirstFramePresented(); }, { once: true });
  video.addEventListener("playing", () => { if (video.videoWidth && video.videoHeight) markFirstFramePresented(); }, { once: true });
  const tryPlay = () => video.play().catch(() => {});
  tryPlay();
  // Mobile browsers (iOS especially) may refuse autoplay until a user
  // gesture; resume on the first tap anywhere.
  document.addEventListener("pointerdown", tryPlay, { once: true });
  video.addEventListener("click", tryPlay);
  hudState.dec = "native video";
}

function startEncodedStreams(receiver, generation = peerGeneration) {
  ensureMainCanvas();
  const { readable } = receiver.createEncodedStreams();
  const reader = readable.getReader();
  (async () => {
    for (;;) {
      const { value: chunk, done } = await reader.read();
      if (done) break;
      if (generation !== peerGeneration) { try { chunk.close(); } catch {} break; }
      const metadata = typeof chunk.getMetadata === "function" ? chunk.getMetadata() : {};
      const wrapped = {
        type: chunk.type,
        timestamp: normalizeEncodedTimestamp(chunk.timestamp, metadata),
        rtpTimestamp: metadata.rtpTimestamp,
        data: chunk.data,
        generation: latencyGuard.generation,
      };
      if (!latencyGuard.acceptWorkerFrame(wrapped)) { hudState.dropped++; continue; }
      latencyGuard.releaseWorkerFrame();
      feedDecoder(wrapped, wrapped.generation);
    }
  })().catch((e) => { hudState.dec = `read:${e}`; });
  hudState.dec = "streams (periodic IDR fallback)";
}

function requestKeyframe(reason) {
  // RTCRtpScriptTransformer's request API is only callable in its Dedicated
  // Worker. Legacy createEncodedStreams keeps the host's periodic IDR path.
  if (encodedWorker) {
    encodedWorker.postMessage({ type: "request-keyframe", reason });
    hudState.dec = `waiting keyframe (${reason})`;
  } else {
    hudState.dec = `waiting periodic IDR (${reason})`;
  }
}

function dropAndResync(reason) {
  hudState.dropped++;
  latencyGuard.reset(reason);
}

// transition: a WebRTC rtpGate ({ token, admittedRtpTimestamp }) the worker
// enforces on its next keyframe and echoes in stats.transitionToken.
function resyncPipeline(reason, transition = null) {
  hudState.resyncs++;
  resetRenderPacing();
  if (decoder) {
    try { decoder.close(); } catch { /* already closed */ }
    decoder = null;
  }
  if (encodedWorker) {
    encodedWorker.postMessage({ type: "resync", generation: latencyGuard.generation, reason,
      ...(transition ? { transition: { token: transition.token, admittedRtpTimestamp: transition.admittedRtpTimestamp ?? null, strict: transition.strict === true } } : {}) });
    hudState.dec = `waiting keyframe (${reason})`;
  } else {
    requestKeyframe(reason);
  }
}

document.addEventListener("visibilitychange", () => {
  if (useWebCodecs && document.visibilityState === "visible") latencyGuard.reset("visibility-resume");
});

function followDecoderCodec(codec) {
  const previousCodec = codecSession.codec;
  if (!codecSession.followBitstreamCodec(codec)) return;
  clearWssKeyframeWatchdog();
  wssDecoderEpoch++;
  try { decoder?.close(); } catch {}
  decoder = null;
  // Keep the admitted keyframe's generation and ordering gate. A full resync
  // would request another keyframe and discard the one that proves the codec.
  while (latencyGuard.decodeFrames > 0) latencyGuard.releaseDecode();
  resetRenderPacing();
  wssDecodeSequenceByTimestamp.clear();
  wssFrameMeta = new WeakMap();
  wssPresentedFrameTraces.clear();
  clearWssHybridCaches();
  wssNoOutputFailures = 0;
  if (wssRecoveryState === "awaiting-keyframe-output") wssRecoveryState = "awaiting-keyframe";
  viewerLog("info", "decoder-keyframe-codec-change", { previousCodec, codec });
}

function feedDecoder(chunk, generation) {
  // An encoded frame is the concrete RTP/media arrival signal. Do not cancel
  // auto fallback merely because a track object was announced.
  rtcStartupFallback?.stop();
  if (generation !== latencyGuard.generation) { hudState.dropped++; return; }
  const data = new Uint8Array(chunk.data);
  const isKey = chunk.type === "key";
  if (!usingWssMedia && firstFrameHandshake.requireAdmission) {
    if (!firstFrameHandshake.admitted) {
      hudState.dropped++;
      return;
    }
    if (transitionNeedsKeyframe) {
      const verdict = transitionKeyframeVerdict({ isKey, frameRtpTimestamp: chunk.rtpTimestamp,
        admittedRtpTimestamp: firstFrameHandshake.rtpGate?.admittedRtpTimestamp, strict: firstFrameHandshake.rtpGate?.strict === true });
      if (verdict !== "admit") {
        hudState.dropped++;
        if (verdict === "unprovable") recoverRtcInPlaceTransition("no-frame-rtp-timestamp");
        return;
      }
      transitionNeedsKeyframe = false;
      transitionGatePassedToken = firstFrameHandshake.requestId;
    }
  }

  const bitstreamCodec = usingWssMedia ? null : detectAnnexBCodec(data, { key: isKey });
  const codecName = bitstreamCodec || chunk.codec || codecSession.codec;
  if (codecName !== codecSession.codec) {
    if (!isKey || !bitstreamCodec) { hudState.dropped++; return; }
    followDecoderCodec(codecName);
  }

  if (!decoder) {
    // WSS HEVC is Annex-B and has no decoder description/config record.
    const codec = codecName === "hevc" ? activeWssHevcCodec() : (avcCodecFromSps(data) || "avc1.64002A");
    resetRenderPacing();
    const decoderEpoch = ++wssDecoderEpoch;
    decoder = new VideoDecoder({
      output: (frame) => {
        if (decoderEpoch !== wssDecoderEpoch) { try { frame.close(); } catch {} return; }
        clearWssKeyframeWatchdog();
        wssNoOutputFailures = 0;
        if (usingWssMedia && !debugFirstDecoderOutput) {
          debugFirstDecoderOutput = true;
          debugTrace("decoder-first-output", { timestamp: frame.timestamp });
        }
        if (usingWssMedia) {
          let metadata = wssDecodeSequenceByTimestamp.get(frame.timestamp);
          if (metadata !== undefined) {
            wssDecodeSequenceByTimestamp.delete(frame.timestamp);
          } else if (wssDecodeSequenceByTimestamp.size) {
            // Some Chromium builds normalize WebCodecs output timestamps by
            // a few microseconds. WSS frames are decoded in order, so the
            // oldest pending metadata is the safe association fallback.
            const first = wssDecodeSequenceByTimestamp.entries().next().value;
            if (first) {
              metadata = first[1];
              wssDecodeSequenceByTimestamp.delete(first[0]);
              if (!wssTimestampFallbackLogged) {
                wssTimestampFallbackLogged = true;
                viewerLog("warn", "wss-timestamp-association-fallback", { frameTimestamp: frame.timestamp, metadataTimestamp: first[0] });
              }
            }
          }
          if (metadata !== undefined) {
            metadata.frameTrace.decodedAtMs = performance.now();
            wssFrameMeta.set(frame, metadata);
            if (usingWssMedia) wssV4.decodedSeq = metadata.sequence >>> 0;
          }
          if (metadata?.key && wssRecoveryState === "awaiting-keyframe-output") {
            wssRecoveryPending = false;
            wssRecoveryState = "healthy";
          }
        }
        latencyGuard.releaseDecode(); onDecodedFrame(frame);
      },
      error: (e) => {
        if (decoderEpoch !== wssDecoderEpoch) return;
        viewerLog("error", "decoder-error", { message: String(e?.message || e) });
        hudState.dec = `err:${e.message}`;
        if (usingWssMedia) noteWssFailure("decode-error"); else dropAndResync("decoder-error");
      },
    });
    try {
      decoder.configure({
        codec,
        optimizeForLatency: true,
        hardwareAcceleration: "prefer-hardware",
      });
      hudState.dec = `${codec}`;
      if (usingWssMedia && !debugFirstDecoderConfigured) {
        debugFirstDecoderConfigured = true;
        debugTrace("decoder-configured", { codec });
      }
    } catch (e) {
      viewerLog("error", "decoder-config-error", { message: String(e?.message || e), codec });
      hudState.dec = `cfg:${e.message}`;
      try { decoder.close(); } catch {}
      decoder = null;
      if (usingWssMedia) noteWssFailure("configure-unsupported");
      return;
    }
  }

  if (decoder.state !== "configured") return;
  if (!isKey && !decoder.sawKeyframe) {
    hudState.dropped++;
    if (usingWssMedia) beginWssRecovery("delta-before-keyframe");
    else requestKeyframe("delta-before-keyframe");
    return;
  }
  decoder.sawKeyframe = decoder.sawKeyframe || isKey;
  const decodeLimit = usingWssMedia ? (streamModeValue() === "game" ? 4 : 8) : 2;
  const decodeHardLimit = usingWssMedia ? decodeLimit + 2 : decodeLimit;
  // v4 never drops a chunk for decoder backpressure: every frame is decoded
  // and only presentation is skipped while the decoder catches up.
  if (!latencyGuard.admitDecode(usingWssMedia ? Infinity : decodeHardLimit)) {
    if (usingWssMedia) { hudState.dropped++; beginWssRecovery("decoder-backpressure"); }
    else dropAndResync("decoder-backpressure");
    return;
  }

  try {
    if (chunk.frameTrace) chunk.frameTrace.decodeQueuedAtMs = performance.now();
    decoder.decode(new EncodedVideoChunk({
      type: isKey ? "key" : "delta",
      timestamp: chunk.timestamp,
      data,
    }));
    hudState.queued = decoder.decodeQueueSize;
  } catch (e) {
    latencyGuard.releaseDecode();
    viewerLog("error", "decoder-decode-throw", {
      message: String(e?.message || e).slice(0, 512),
      codec: chunk.codec,
      type: isKey ? "key" : "delta",
      byteLength: data.byteLength,
    });
    hudState.dec = `dec:${e.message}`;
    if (usingWssMedia) noteWssFailure("decode-throw"); else dropAndResync("decoder-throw");
  }
}

function drawFrame(frame, due = performance.now()) {
  try {
    if (!desktopAvailable) return;
    const w = frame.displayWidth, h = frame.displayHeight;
    lastDecodedDimensions = { width: w, height: h };
    const metadata = usingWssMedia ? wssFrameMeta.get(frame) : null;
    const view = metadata ? viewportForGeneration(metadata.generation) : null;
    if (usingWssMedia && (!metadata || !view)) return; // never render a generation with stale geometry
    let hybridTilesDrawn = 0;
    let presentedCanvas = screen;
    let presentedContext = ctx;
    if (usingWssMedia && view?.enabled) {
      if (sharpScreen.width !== w || sharpScreen.height !== h) {
        sharpScreen.width = w;
        sharpScreen.height = h;
      }
      const context = ensureSharpCanvas();
      presentedCanvas = sharpScreen;
      presentedContext = context;
      hybridTilesDrawn = activeWssHybridMode()
        ? drawBaseThenHybrid(context, frame, activeWssHybridCache(), metadata, w, h)
        : (context.drawImage(frame, 0, 0, w, h), 0);
      layoutSharpCanvas(view);
      const gain = cropGain(view, w, h);
      hudState.dimensions = `${view.width}x${view.height}→${w}x${h} ${gain.x.toFixed(1)}x/${gain.y.toFixed(1)}x`;
    } else {
      if (screen.width !== w || screen.height !== h) {
        screen.width = w;
        screen.height = h;
        syncViewportAndFollow();
      }
      const context = ensureMainCanvas();
      presentedContext = context;
      hybridTilesDrawn = activeWssHybridMode()
        ? drawBaseThenHybrid(context, frame, activeWssHybridCache(), metadata, w, h)
        : (context.drawImage(frame, 0, 0, w, h), 0);
      layoutSharpCanvas(null);
      hudState.dimensions = `${w}x${h}`;
    }
    // Mark only after the frame has been copied to its visible canvas. Codec
    // labels and host admission alone are not enough to hide startup UI.
    markFirstFramePresented(metadata?.generation ?? null, metadata?.cropRevision ?? null,
      { gatePassed: transitionGatePassedToken !== null && transitionGatePassedToken === firstFrameHandshake.requestId });
    if (usingWssMedia) framePresentationTelemetry.noteImmediatePresented();
    const renderedAt = performance.now();
    if (metadata?.frameTrace) {
      metadata.frameTrace.hybridTilesDrawn = hybridTilesDrawn;
      metadata.frameTrace.presentedAtMs = renderedAt;
      wssLatestFrameTrace = { ...metadata.frameTrace };
      wssPresentedFrameTraces.set(metadata.frameTrace.sequence >>> 0, { ...metadata.frameTrace });
      while (wssPresentedFrameTraces.size > 64)
        wssPresentedFrameTraces.delete(wssPresentedFrameTraces.keys().next().value);
    }
    if (lastFrameDrawnAt !== null) {
      const frameGap = Math.max(0, Math.round(renderedAt - lastFrameDrawnAt));
      hudState.frameGap = frameGap;
      hudState.maxFrameGap = Math.max(Number(hudState.maxFrameGap) || 0, frameGap);
    }
    lastFrameDrawnAt = renderedAt;
    lastRenderedTimestamp = frame.timestamp;
    lastRenderedDueMs = due;
    hudState.lag = Math.max(0, Math.round(renderedAt - due));
    fpsWindow++;
    framesDrawn++;
    hudState.presented = framesDrawn;
    if (usingWssMedia && metadata !== undefined) wssPresentedSequence = metadata.sequence >>> 0;
  } finally {
    try { frame.close(); } catch {}
  }
}

// ---- input: pointer lock mouse ----------------------------------------------
const BM = { 0: 1, 1: 4, 2: 2 }; // e.button -> bitmask (left, middle, right)
let buttons = 0, touchButtons = 0;
let locked = false;
let lastNonMousePointerAt = -Infinity;

// The media inside an object-fit: contain element (a <video> or the canvas).
function videoContentRect(video) {
  const box = video.getBoundingClientRect();
  const aspect = (video.videoWidth || video.width) / (video.videoHeight || video.height);
  if (!Number.isFinite(aspect) || aspect <= 0 || box.width === 0 || box.height === 0) return box;
  const boxAspect = box.width / box.height;
  if (aspect > boxAspect) {
    const height = box.width / aspect;
    return { left: box.left, top: box.top + (box.height - height) / 2, width: box.width, height };
  }
  const width = box.height * aspect;
  return { left: box.left + (box.width - width) / 2, top: box.top, width, height: box.height };
}

async function requestPointerLock() {
  if (pointerMode !== "game") return;
  try {
    await inputSurface.requestPointerLock({ unadjustedMovement: true });
    locked = true;
  } catch {
    try {
      inputSurface.requestPointerLock();
      locked = true;
    } catch { locked = false; }
  }
}
document.addEventListener("pointerlockchange", () => {
  locked = document.pointerLockElement === inputSurface;
  updatePointerHud();
  if (!locked) releaseInput();
});
document.addEventListener("click", (event) => {
  if (pointerMode === "game" && event.target === inputSurface && event.detail && event.pointerType !== "touch" && event.pointerType !== "pen" && !event.sourceCapabilities?.firesTouchEvents && performance.now() - lastNonMousePointerAt > 1500 && !locked && connected) requestPointerLock();
});
document.addEventListener("mousemove", (e) => {
  if (!inputReady || !connected) return;
  if (pointerMode === "game" && locked) {
    // Relative (unadjusted) deltas.
    updateRemoteCursor(cursor => cursor.moveScreen(e.movementX, e.movementY, viewport));
    queueMotion(e.movementX, e.movementY);
  } else {
    if (pointerMode !== "desktop") return;
    if (e.target !== inputSurface) return;
    // Absolute fallback over the displayed video (not its letterbox): 0..65535.
    const r = videoContentRect(activeVideo || screenMedia());
    if (r.width === 0 || r.height === 0) return;
    const x = clamp((e.clientX - r.left) / r.width, 0, 1);
    const y = clamp((e.clientY - r.top) / r.height, 0, 1);
    updateRemoteCursor(cursor => cursor.setNormalized(x, y));
    locallyAuthoritativeCursor = true; mousePointerActive = true; queueAbsolute(x * 65535, y * 65535); showTouchCursor();
  }
});
document.addEventListener("mousedown", (e) => {
  if (!inputReady) return;
  if (e.target !== inputSurface) return;
  inputSurface.focus({ preventScroll: true });
  // The initial capture gesture is local-only; never turn it into a remote click.
  if (pointerMode === "game" && !locked && connected) { requestPointerLock(); e.preventDefault(); return; }
  buttons |= BM[e.button] || (e.button === 3 ? 8 : e.button === 4 ? 16 : 0);
  if (pointerMode === "desktop") setTouchButtonsAtCursor(buttons); else input.buttons(buttons);
  e.preventDefault();
});
document.addEventListener("mouseup", (e) => {
  if (!inputReady) return;
  if (e.target !== inputSurface) return;
  buttons &= ~(BM[e.button] || (e.button === 3 ? 8 : e.button === 4 ? 16 : 0));
  if (pointerMode === "desktop") setTouchButtonsAtCursor(buttons); else input.buttons(buttons);
  e.preventDefault();
});
document.addEventListener("contextmenu", (e) => e.preventDefault());
document.addEventListener("wheel", (e) => {
  if (e.target !== inputSurface) return;
  if (e.ctrlKey) { syncViewport(); updateRemoteCursor(cursor => cursor.setFromScreen({x:e.clientX,y:e.clientY}, viewport)); setZoom(viewport.zoom + (e.deltaY < 0 ? .1 : -.1), {x:e.clientX,y:e.clientY}); e.preventDefault(); return; }
  if (!inputReady) return;
  // Accumulate high-resolution deltas into WHEEL_DELTA units (120 per notch),
  // carrying fractions so smooth scrolling never loses distance.
  const units = wheelAccumulator.add(e.deltaX * settings.scrollSpeed, e.deltaY * settings.scrollSpeed, e.deltaMode);
  if (units.x || units.y) queueMotion(0, 0, units.x, units.y);
  e.preventDefault();
}, { passive: false });
const wheelAccumulator = new WheelAccumulator();

// ---- input: clipboard text/images + keyboard lock ----------------------------------
let hostInputCapabilities = { scancode: false, clipboard: false };
const remoteClipboard = new ClipboardReceiver();
const remoteClipboardButton = document.getElementById("remote-clipboard");
const remoteClipboardWriter = new RemoteClipboardWriter({ clipboard: navigator.clipboard, ClipboardItem: globalThis.ClipboardItem,
  autoCopy: settings.clipboardAutoCopy,
  onPending: pending => { if (remoteClipboardButton) remoteClipboardButton.hidden = !pending; } });
let pasteArmed = null;
let clipboardTransferId = 0;
function applyInputCapabilities(msg) {
  hostInputCapabilities = { scancode: msg?.protocol === 1 && msg.scancode === true, clipboard: msg?.protocol === 1 && msg.clipboard === true };
  input.scancode = hostInputCapabilities.scancode;
  if (!hostInputCapabilities.clipboard) clearRemoteClipboard();
}
// Clipboard sync is text-only and never runs on the Windows secure desktop.
function clipboardSyncEnabled() { return hostInputCapabilities.clipboard && desktopKind !== "secure" && inputReady; }
function clearRemoteClipboard() { remoteClipboardWriter.clear(); remoteClipboard.reset(); }
function discardPendingClipboard() { remoteClipboardWriter.discardPending(); remoteClipboard.reset(); }
function receiveRemoteClipboard(msg) {
  if (!hostInputCapabilities.clipboard || desktopKind === "secure") return;
  const content = remoteClipboard.push(msg);
  if (content === null) return;
  remoteClipboardWriter.receive(content, document.hasFocus());
}
remoteClipboardButton.onclick = e => { e.stopPropagation(); remoteClipboardWriter.write(); inputSurface.focus?.(); };
window.addEventListener("focus", () => { if (desktopKind !== "secure") remoteClipboardWriter.writeAutomatically(true); });
window.addEventListener("blur", () => remoteClipboardWriter.writeAutomatically(false));
document.addEventListener("copy", clearRemoteClipboard, true);
document.addEventListener("cut", clearRemoteClipboard, true);
function isPasteShortcut(e) { return (e.ctrlKey || e.metaKey) && !e.altKey && (e.key === "v" || e.key === "V"); }
// Ctrl/Cmd+V: let the browser raise a paste event carrying the local
// clipboard, then set the host clipboard and paste there. Without clipboard
// text (or permission) the plain key goes through and pastes the host's own.
function armBrowserPaste() {
  discardPendingClipboard();
  clearTimeout(pasteArmed);
  pasteArmed = setTimeout(() => { pasteArmed = null; sendPhysicalTap("KeyV"); }, 300);
}
function sendHostClipboardPaste() {
  if (!inputReady) return;
  pasteHostClipboard(input, held);
}
function sendPhysicalTap(code) { if (!inputReady) return; input.physicalKey(code, false); input.physicalKey(code, true); }
document.addEventListener("paste", e => {
  discardPendingClipboard();
  if (pasteArmed === null) return;
  clearTimeout(pasteArmed); pasteArmed = null;
  e.preventDefault();
  // File transfer hook (4/5): pasted files or an image upload and paste on the PC.
  const pastedFiles = fileTransferAvailable() ? filesFromPaste(e) : [];
  if (pastedFiles.length) {
    const snapshot = remoteClipboardWriter.latest, pasteEpoch = mediaEpoch;
    if (pastedFiles.length === 1 && snapshot?.mimeType === "image/png") {
      sameClipboardImage(snapshot, pastedFiles[0]).then(matches => {
        if (!inputReady || desktopKind === "secure" || mediaEpoch !== pasteEpoch) return;
        if (matches && remoteClipboardWriter.latest === snapshot && remoteClipboardWriter.pending === null) sendHostClipboardPaste();
        else fileClient.upload(pastedFiles, "paste");
      });
    } else fileClient.upload(pastedFiles, "paste");
    return;
  }
  const text = e.clipboardData?.getData("text/plain") || "";
  clipboardTransferId = (clipboardTransferId + 1) & 0xffff;
  if (!text || !clipboardSyncEnabled() || !input.clipboard(text, clipboardTransferId)) sendPhysicalTap("KeyV");
}, true);
// ---- file transfer (hook 5/5) ----------------------------------------------
// Protocol and state: file-transfer.js / file-client.js; rendering: file-ui.js.
const fileClient = new FileTransferClient({ socketUrl: fileSocketUrl, socketTicket: () => signalingTicket });
let nativeFileDropAvailable = false;
let nativeFileDropPreviewAvailable = false;
const fileDragPreview = createFileDragPreviewController({
  send: sendControl,
  getContext: () => ({ available: nativeFileDropPreviewAvailable && nativeFileDropAvailable && fileTransferAvailable() && connected, epoch: mediaEpoch }),
  resolveDrop: resolveNativeFileDrop,
});
const fileUi = installFileTransferUi({
  client: fileClient,
  root: document.getElementById("file-transfer"),
  veil: document.getElementById("file-drop-veil"),
  resolveDrop: resolveNativeFileDrop,
  onDragPreview: (event, target) => fileDragPreview.update(event, target),
  onDragPreviewEnd: () => fileDragPreview.cancel(),
  onError: error => viewerLog("warn", "file-download-target", { message: String(error?.message || error) }),
});
function fileSocketUrl() {
  if (!mediaEpoch || !signalingPc || !signalingTicket || !authenticatedSession?.mediaReady) return null;
  const query = new URLSearchParams({ epoch: mediaEpoch });
  query.set("pc", signalingPc);
  return `${proto}//${location.host}/file-ws?${query}`;
}
let fileEpoch = null;
function noteFileEpoch(epoch) {
  if (epoch !== fileEpoch) { fileDragPreview.cancel(); fileClient.reset("disconnected"); }
  fileEpoch = epoch;
}
function fileTransferAvailable() { return fileUi.enabled && desktopKind !== "secure" && inputReady; }
function setFileTransferAvailable(available) {
  if (!available && fileUi.enabled) fileClient.reset("disconnected");
  fileUi.setEnabled(available);
}
function resolveNativeFileDrop(event) {
  // Keep the existing save route on touch devices, outside the remote surface,
  // and when connected to hosts implementing only the original file protocol.
  if (!nativeFileDropAvailable || mobilePointerDevice || pointerMode !== "desktop" ||
      event.pointerType === "touch" || event.sourceCapabilities?.firesTouchEvents || event.target !== inputSurface) return { purpose: "save" };
  if (!fileTransferAvailable() || !connected || !desktopAvailable || desktopAwaitingFrame ||
      desktopInputState !== "enabled" || !hasPresentedFrame || viewportPending || transitionNeedsKeyframe) return { error: "drop-not-ready" };
  // The base full-desktop canvas remains the coordinate plane while a sharp
  // crop is overlaid. A committed crop without its matching rendered overlay
  // is transitional geometry and must not become a native drop target.
  if (viewportState.enabled && (!renderedSharpView || renderedSharpView.revision !== viewportState.revision ||
      renderedSharpView.generation !== fileDropPresentedGeneration)) return { error: "drop-not-ready" };
  const media = activeVideo || screenMedia();
  const width = activeVideo ? activeVideo.videoWidth : screenDims().width;
  const height = activeVideo ? activeVideo.videoHeight : screenDims().height;
  if (!(width > 0 && height > 0)) return { error: "drop-not-ready" };
  const area = streamArea();
  const drop = mapFileDropPoint({ x: event.clientX, y: event.clientY }, videoContentRect(media), area);
  if (!drop) return { purpose: "save" };
  const generation = fileDropPresentedGeneration;
  if (!Number.isInteger(generation) || generation <= 0 || generation > 0xffff_ffff) return { error: "drop-not-ready" };
  return { purpose: "drop", drop: { ...drop, generation,
    viewportRevision: fileDropPresentedViewportRevision ?? (viewportState.revision || 0) } };
}
function updateKeyboardLock() {
  const keyboard = navigator.keyboard;
  if (!keyboard?.lock) return;
  // Fullscreen streams receive system shortcuts (Ctrl+W, Alt+Tab where the
  // browser allows it, Esc) instead of the local browser.
  if (document.fullscreenElement && settings.keyboardLock) keyboard.lock().catch(() => {}); else keyboard.unlock?.();
}
document.addEventListener("fullscreenchange", updateKeyboardLock);

// ---- input: keyboard (e.code -> set-1 scan code; VK for legacy hosts) -------
// The physical key is sent, never the character: the host keyboard layout
// produces the text, so AltGr and non-US layouts behave natively. See
// keyDescriptor/SCAN_CODES in input.js.

const held = new Set();
function keyboardActive() {
  const active = document.activeElement;
  const keyboard = document.getElementById("keyboard-input");
  const localControlFocused = active !== keyboard && Boolean(active?.closest?.("#session-strip, #strip-handle, .bp-popover, .bp-panel, #context-hint, #desktop-state"));
  return shouldCaptureKeyboard({ inputReady, pointerMode, locked,
    surfaceFocused: active === inputSurface, keyboardFocused: active === keyboard, localControlFocused });
}
document.addEventListener("keydown", (e) => {
  if ((e.ctrlKey || e.metaKey) && !e.altKey && ["c", "C", "x", "X"].includes(e.key)) clearRemoteClipboard();
  if (isPasteShortcut(e)) discardPendingClipboard();
  if (!inputReady || !keyboardActive()) return;
  if (e.target?.id === "keyboard-input") {
    // Let the native field own text editing. Its resulting value is mirrored
    // by the reconciliation listener below, which avoids duplicate Backspace
    // packets and preserves mobile autocorrect/composition behavior.
    const localEditKey = ["Backspace", "Delete", "ArrowLeft", "ArrowRight", "ArrowUp", "ArrowDown", "Home", "End"];
    if (localEditKey.includes(e.key) && !e.ctrlKey && !e.altKey && !e.metaKey) return;
    if (e.key?.length === 1 && !e.ctrlKey && !e.altKey && !e.metaKey) return;
  }
  if (!keyDescriptor(e.code)) return;
  if (isPasteShortcut(e) && (clipboardSyncEnabled() || fileTransferAvailable())) {
    // Leave the event to the browser so it fires `paste`; see armBrowserPaste.
    if (!e.repeat) armBrowserPaste();
    return;
  }
  e.preventDefault();
  if (held.has(e.code)) {
    // Typematic repeat: Windows does not auto-repeat injected keys, so resend the press.
    if (e.repeat) input.physicalKey(e.code, false);
    return;
  }
  if (e.repeat) return;
  held.add(e.code);
  input.physicalKey(e.code, false);
});
document.addEventListener("keyup", (e) => {
  if (!keyDescriptor(e.code)) return;
  if (!keyboardActive() && !held.has(e.code)) return;
  e.preventDefault();
  if (!held.has(e.code)) return;
  held.delete(e.code);
  input.physicalKey(e.code, true);
});
const keyboardInput = document.getElementById("keyboard-input");
const keyboardStatus = document.getElementById("keyboard-status");
let keyboardShadow = "";
let keyboardComposing = false;
// BEGIN secure-composition-gate
function createCompositionCommitGate(commit, schedule = setTimeout, cancel = clearTimeout) {
  let pending = null;
  return {
    stage(text) {
      this.reset();
      const value = String(text ?? "");
      if (!value) return false;
      const transaction = { value, timer: null };
      transaction.timer = schedule(() => {
        if (pending !== transaction) return;
        pending = null;
        commit(transaction.value);
      }, 50);
      pending = transaction;
      return true;
    },
    consume(text) {
      if (!pending) return false;
      const transaction = pending;
      pending = null;
      cancel(transaction.timer);
      commit(String(text ?? "") || transaction.value);
      return true;
    },
    reset() {
      if (pending) cancel(pending.timer);
      pending = null;
    },
  };
}
// END secure-composition-gate
const secureCompositionCommit = createCompositionCommitGate(text => {
  if (!inputReady || desktopKind !== "secure") { resetKeyboardDraft(); return; }
  handleSecureKeyboardInput({ data: text, inputType: "insertCompositionText" });
});
const modifierVks = new Map([["Ctrl", 0x11], ["Alt", 0x12], ["Shift", 0x10], ["Win", 0x5B]]);
// Keys popover (touch), after the Android app: navigation and editing keys,
// then function keys. Arrows live in their own popover.
const specialKeys = [
  ["Esc", 0x1B], ["Tab", 0x09], ["Home", 0x24], ["End", 0x23], ["PgUp", 0x21], ["PgDn", 0x22],
  ["Enter", 0x0D], ["Bksp", 0x08], ["Del", 0x2E], ["Ins", 0x2D], ["Space", 0x20], ["Menu", 0x5D],
  ...Array.from({ length: 12 }, (_, i) => [`F${i + 1}`, 0x70 + i]),
];
// Arrow pad: [label, vk, repeats while held, small label].
const arrowPad = [
  ["Home", 0x24, false, true], ["↑", 0x26, true], ["PgUp", 0x21, true, true],
  ["←", 0x25, true], ["Enter", 0x0D, false, true], ["→", 0x27, true],
  ["End", 0x23, false, true], ["↓", 0x28, true], ["PgDn", 0x22, true, true],
];
const modifierNames = new Map([[162, "Ctrl"], [164, "Alt"], [160, "Shift"], [91, "Win"]]);
function renderKeyboardModifiers() {
  document.querySelectorAll(".modifier[data-vk]").forEach(button => {
    const held = latchedModifiers.has(Number(button.dataset.vk));
    button.classList.toggle("active", held);
    button.setAttribute("aria-pressed", String(held));
  });
  const names = [...modifierNames].filter(([vk]) => latchedModifiers.has(vk)).map(([, name]) => name);
  const latch = document.getElementById("latch-label");
  if (latch) latch.textContent = names.map(name => name[0]).join("");
  document.getElementById("strip-keys")?.setAttribute("aria-label", names.length ? `Keys, holding ${names.join("+")}` : "Keys");
}
function releaseLatchedModifiers() {
  for (const vk of latchedModifiers) input.key(vk, true);
  latchedModifiers.clear();
  renderKeyboardModifiers();
}
function sendSpecialKey(vk) {
  if (!inputReady) return;
  input.key(vk, false); input.key(vk, true);
  releaseLatchedModifiers();
}
document.querySelectorAll(".modifier[data-vk]").forEach(button => {
  button.onclick = () => {
    if (!inputReady) return;
    const vk = Number(button.dataset.vk);
    if (latchedModifiers.has(vk)) { input.key(vk, true); latchedModifiers.delete(vk); }
    else { input.key(vk, false); latchedModifiers.add(vk); }
    renderKeyboardModifiers();
  };
});
for (const [label, vk] of specialKeys) {
  const button = document.createElement("button");
  button.type = "button"; button.className = "key"; button.textContent = label; button.dataset.vk = String(vk);
  button.onclick = () => sendSpecialKey(vk);
  document.getElementById("special-key-grid").append(button);
}
document.getElementById("clear-modifiers").onclick = () => releaseLatchedModifiers();
// Held arrow keys repeat like a physical key: down, repeated downs, then up.
let heldPadKey = null;
function stopPadRepeat() {
  if (!heldPadKey) return;
  clearTimeout(heldPadKey.timer);
  if (inputReady) input.key(heldPadKey.vk, true);
  heldPadKey = null;
  releaseLatchedModifiers();
}
for (const [label, vk, repeats, small] of arrowPad) {
  const button = document.createElement("button");
  button.type = "button"; button.className = `key${small ? " key-small" : ""}`; button.textContent = label; button.dataset.vk = String(vk);
  button.setAttribute("aria-label", { "↑": "Up", "↓": "Down", "←": "Left", "→": "Right" }[label] || label);
  if (!repeats) button.onclick = () => sendSpecialKey(vk);
  else {
    button.addEventListener("pointerdown", event => {
      event.preventDefault();
      stopPadRepeat();
      if (!inputReady) return;
      input.key(vk, false);
      const tick = () => { if (heldPadKey?.vk !== vk || !inputReady) return; input.key(vk, false); heldPadKey.timer = setTimeout(tick, 60); };
      heldPadKey = { vk, timer: setTimeout(tick, 400) };
    });
    for (const type of ["pointerup", "pointercancel", "pointerleave"]) button.addEventListener(type, stopPadRepeat);
    // Keyboard activation (Enter/Space) taps once.
    button.addEventListener("click", event => { if (event.detail === 0) sendSpecialKey(vk); });
  }
  document.getElementById("arrow-pad").append(button);
}
// Shortcuts popover: one row per chord, sent as physical keys.
function sendChord(keys) {
  if (!inputReady) return false;
  for (const [code, up] of chordSteps(keys)) input.physicalKey(code, up);
  return true;
}
function renderShortcutList() {
  const list = document.getElementById("shortcut-list");
  list.replaceChildren(...shortcutsFor(chromeLayout()).map(shortcut => {
    const row = document.createElement("button");
    row.type = "button";
    row.className = "pop-row";
    row.dataset.shortcut = shortcut.id;
    const title = document.createElement("span");
    title.className = "pop-row-title";
    title.textContent = shortcut.label;
    const keys = document.createElement("span");
    keys.className = "pop-row-trail";
    keys.textContent = chordLabel(shortcut.keys);
    row.append(title, keys);
    row.setAttribute("aria-label", `${shortcut.label}, ${shortcut.keys.map(keyName).join(" ")}`);
    row.onclick = () => { sendChord(shortcut.keys); popovers.close(); };
    return row;
  }));
}
function resetKeyboardDraft() {
  keyboardComposing = false;
  secureCompositionCommit.reset();
  keyboardShadow = "";
  keyboardInput.value = "";
}
function sendKeyboardText(text) {
  let chunk = "";
  let sent = 0;
  for (const character of String(text)) {
    const candidate = chunk + character;
    if (new TextEncoder().encode(candidate).byteLength > 500 && chunk) {
      if (input.text(chunk)) sent += Array.from(chunk).length;
      chunk = character;
    } else chunk = candidate;
  }
  if (chunk && input.text(chunk)) sent += Array.from(chunk).length;
  return sent;
}
function reconcileKeyboardValue(nextValue) {
  const next = String(nextValue ?? "");
  const edit = reconcileTextEdit(keyboardShadow, next);
   if (inputReady) {
    for (let i = 0; i < edit.deleteCount; i++) {
      input.key(0x08, false);
      input.key(0x08, true);
    }
    const inserted = sendKeyboardText(edit.insert);
    if (edit.deleteCount || inserted) {
      keyboardStatus.textContent = edit.deleteCount
        ? `Applied correction: ${edit.deleteCount} delete${edit.deleteCount === 1 ? "" : "s"}${inserted ? ` + ${inserted} character${inserted === 1 ? "" : "s"}` : ""}.`
        : `Sent ${inserted} character${inserted === 1 ? "" : "s"}.`;
    }
  }
  keyboardShadow = next;
}
function handleSecureKeyboardInput(event) {
  if (desktopInputState !== "enabled") { resetKeyboardDraft(); return; }
  if (event.inputType === "deleteContentBackward") {
    input.key(0x08, false); input.key(0x08, true);
    keyboardStatus.textContent = "Protected input sent.";
    resetKeyboardDraft();
    return;
  }
  const source = event.data ?? keyboardInput.value;
  const units = secureTextUnits(source, event.inputType || "insertText");
  for (const unit of units) input.text(unit);
  keyboardStatus.textContent = units.length ? "Protected input sent." : "Paste and automatic corrections are disabled on secure desktop.";
  resetKeyboardDraft();
}
keyboardInput.addEventListener("compositionstart", () => { secureCompositionCommit.reset(); keyboardComposing = true; });
keyboardInput.addEventListener("input", event => {
  if (!inputReady || event.isComposing || keyboardComposing) return;
  if (desktopKind === "secure" && secureCompositionCommit.consume(event.data ?? keyboardInput.value)) return;
  if (desktopKind === "secure") { handleSecureKeyboardInput(event); return; }
  reconcileKeyboardValue(keyboardInput.value);
});
keyboardInput.addEventListener("compositionend", event => {
  keyboardComposing = false;
  if (desktopKind === "secure") {
    if (inputReady) secureCompositionCommit.stage(event.data ?? keyboardInput.value);
    else resetKeyboardDraft();
    return;
  }
  if (inputReady) reconcileKeyboardValue(keyboardInput.value);
});
window.addEventListener("blur", () => {
  releaseInput(); held.clear(); resetKeyboardDraft();
});
document.addEventListener("visibilitychange", () => { if (document.visibilityState === "hidden") { releaseInput(); resetTouchCursor(); } });

// TeamViewer-style Mouse mode: the screen is a touchpad. A predicted cursor
// responds locally while the real input travels over the low-latency motion
// lane, and every local zoom path keeps that cursor as its focus.
const gestures = new TouchGesture();
const viewport = new Viewport(innerWidth, innerHeight);
const remoteCursor = new RemoteCursor();
cursorReconciler = new CursorReconciler(remoteCursor);
const touchCursor = document.getElementById("touch-cursor");
const cursorAppearance = new BrowserCursorAppearance(inputSurface, touchCursor);
const mobilePointerDevice = navigator.maxTouchPoints > 0 && matchMedia("(any-pointer:coarse)").matches;
const pointerModeSelect = document.getElementById("setting-pointer-mode");
const qualitySelect = document.getElementById("quality");
let pointerMode = settings.pointerMode === "game" ? "game" : "desktop";
let cursorSyncTimer = null;
let locallyAuthoritativeCursor = false;
let lastCursorCorrectionAt = -Infinity;
// v0 captures one 1920x1080 desktop. Query overrides keep touch sensitivity
// correct for a manually selected monitor until native dimensions are signaled.
const remoteDesktop = {
  width: clamp(Number(params.get("remoteWidth")) || 1920, 320, 16384),
  height: clamp(Number(params.get("remoteHeight")) || 1080, 240, 16384),
};
let touchCursorVisible = false;
// A real mouse uses the native pointer; the drawn overlay is for touch/pen.
let mousePointerActive = false;
const coarsePointerQuery = matchMedia("(any-pointer:coarse)");
// The strip layout follows the primary pointer: a touch-first device gets the
// bottom bar, a mouse-first one (even with a touchscreen) the top strip.
const touchLayoutQuery = matchMedia("(pointer: coarse)");
function chromeLayout() { return touchLayoutQuery.matches ? "touch" : "desktop"; }
const contextHint = document.getElementById("context-hint");
const contextHintCopy = document.getElementById("context-hint-copy");

function syncPointerCapability() {
  document.body.classList.toggle("coarse-pointer", coarsePointerQuery.matches);
}

function syncShellMode() {
  const gameMode = pointerMode === "game";
  document.body.classList.toggle("mode-game", gameMode);
  document.body.classList.toggle("mode-work", !gameMode);
  const touchMode = document.getElementById("strip-touch-mode");
  touchMode.setAttribute("aria-pressed", String(gameMode));
  touchMode.setAttribute("aria-label", `Touch mode: ${gameMode ? "Game (relative)" : "Work (pointer)"}`);
  touchMode.title = gameMode ? "Touch mode: Game" : "Touch mode: Work";
}

function showContextHint() {
  if (!hasPresentedFrame || !contextHint || !contextHintCopy) return;
  const mode = pointerMode === "game" ? "game" : "work";
  let dismissed = false;
  try { dismissed = localStorage.getItem(`bp_hint_${mode}_dismissed`) === "1"; } catch {}
  contextHint.hidden = dismissed;
  contextHint.dataset.mode = mode;
  contextHintCopy.textContent = mode === "game"
    ? (chromeLayout() === "touch" ? "Drag to move the pointer · tap to click · Touch mode switches back to Work." : "Click the stream to capture the pointer · Esc releases it.")
    : "Drag to move the pointer · pinch to zoom · two-finger tap to right-click.";
}

syncPointerCapability();
coarsePointerQuery.addEventListener?.("change", syncPointerCapability);
function clearCursorSyncTimer() { if (cursorSyncTimer !== null) clearTimeout(cursorSyncTimer); cursorSyncTimer = null; }
 function updatePointerHud() {
   const workMode = pointerMode === "desktop";
   hudState.pointer = workMode ? "work" : "gaming";
   hudState.cursorSeq = `${cursorReconciler.lastSent ?? "…"}/${cursorReconciler.lastAck ?? "…"}`;
   hudState.cursorAge = cursorReconciler.feedbackAt === null ? "…" : Math.round(performance.now()-cursorReconciler.feedbackAt);
   if (pointerModeSelect.value !== pointerMode) pointerModeSelect.value = pointerMode;
   const captureTitle = document.querySelector("#capture .pop-row-title");
   const captureText = locked ? "Release cursor" : "Capture cursor";
   if (captureTitle.textContent !== captureText) captureTitle.textContent = captureText;
   const sessionPointer = document.getElementById("session-pointer");
   const pointerText = workMode ? "Work · absolute pointer" : (locked ? "Game · cursor captured" : "Game · relative, cursor released");
   if (sessionPointer && sessionPointer.textContent !== pointerText) sessionPointer.textContent = pointerText;
   syncShellMode();
 }
function requestCursorSync() {
  clearCursorSyncTimer();
  if (pointerMode !== "desktop" || !inputReady) { updatePointerHud(); return; }
  if (input.cursorSync()) cursorReconciler.noteSent(input.lastCursorSequence);
  updatePointerHud();
  if (!cursorReconciler.synced) cursorSyncTimer = setTimeout(requestCursorSync, 500);
}
function onInputReady() { cursorReconciler.reset(); locallyAuthoritativeCursor=false; touchCursorVisible=shouldShowPredictedCursor({pointerMode,inputReady}); renderTouchCursor(); requestCursorSync(); }
function handleCursorState(data, source = "rtc") {
  if (cursorAppearance.receive(data, { source, controlOpen: !usingWssMedia && dc?.readyState === "open" })) { renderTouchCursor(); return; }
  const state=decodeCursorState(data);
  if (!state || !cursorReconciler) return;
  const applied=cursorReconciler.apply(state,{touchActive:gestures.points.size>0,preserveCursor:pointerMode === "desktop" && locallyAuthoritativeCursor});
  const now=performance.now();
  if (shouldCorrectCursorDrift({pointerMode,locallyAuthoritative:locallyAuthoritativeCursor,state,cursor:remoteCursor,remote:remoteDesktop,now,lastCorrectionAt:lastCursorCorrectionAt})) {
    const point=remoteCursor.absolutePoint();
    if (input.buttonsAt(point.x,point.y,touchButtons || buttons)) { cursorReconciler.noteSent(input.lastCursorSequence); lastCursorCorrectionAt=now; }
  }
  if (cursorReconciler.synced) clearCursorSyncTimer();
  touchCursorVisible=shouldShowCursorOverlay({pointerMode, ...cursorReconciler});
  updatePointerHud();
  if (applied) { centerCameraOnCursor(); renderViewport(); } else renderTouchCursor();
}
function setPointerMode(next, persist=true) {
  next = next === "game" ? "game" : "desktop";
  if (pointerMode === next && persist) return;
  releaseInput(); gestures.reset(); cursorReconciler.reset(); touchCursorVisible=false; pointerMode=next;
  locallyAuthoritativeCursor=false;
  if (pointerMode === "desktop" && document.pointerLockElement) document.exitPointerLock?.();
  if (persist) { settings.pointerMode = pointerMode; persistSettings(); }
  if (persist) sendStreamMode("user-mode");
  clearCursorSyncTimer(); updatePointerHud(); renderTouchCursor();
  if (inputReady) {
    if (input.cursorSync()) cursorReconciler.noteSent(input.lastCursorSequence);
    if (pointerMode === "desktop") cursorSyncTimer=setTimeout(requestCursorSync,500);
   }
   scheduleSharpViewport("pointer-mode");
   showContextHint();
 }
function streamModeValue() { return pointerMode === "game" ? "game" : "work"; }
function sendStreamMode(reason) {
  if (!signalingRegistered || ws?.readyState !== WebSocket.OPEN) return;
  sendControl({ type: "media-control", action: "mode", protocol: WSS_PROTOCOL, epoch: mediaEpoch,
    streamMode: streamModeValue(), reason });
}
// The stream area in window coordinates, without #visual's zoom/pan transform.
function streamArea() { const width = visual.offsetWidth || innerWidth, height = visual.offsetHeight || innerHeight; return { left: visual.offsetLeft, top: visual.offsetTop, width, height }; }
function containedContentRect() { const area = streamArea(), areaWidth = area.width, areaHeight = area.height, mediaWidth = activeVideo?.videoWidth || screenDims().width, mediaHeight = activeVideo?.videoHeight || screenDims().height; if (!mediaWidth || !mediaHeight) return {x:area.left,y:area.top,width:areaWidth,height:areaHeight}; const scale=Math.min(areaWidth/mediaWidth,areaHeight/mediaHeight), width=mediaWidth*scale, height=mediaHeight*scale; return {x:area.left+(areaWidth-width)/2,y:area.top+(areaHeight-height)/2,width,height}; }
function syncViewport() { const area = streamArea(); viewport.resize(area.width, area.height, area.left, area.top); viewport.setContent(containedContentRect()); renderTouchCursor(); }
function renderTouchCursor() { cursorAppearance.render({ pointerMode, inputReady, point: remoteCursor.screenPoint(viewport), overlayVisible: touchCursorVisible, mousePointerActive }); }
function centerCameraOnCursor() { if (shouldCenterCameraOnCursor({ zoom: viewport.zoom, pointerMode, synced: cursorReconciler.synced, locallyAuthoritative: locallyAuthoritativeCursor })) viewport.centerOnNormalized(remoteCursor.snapshot()); }
function syncViewportAndFollow() { syncViewport(); centerCameraOnCursor(); renderViewport(); }
function updateRemoteCursor(update) { update(remoteCursor); if (pointerMode === "desktop") locallyAuthoritativeCursor=true; centerCameraOnCursor(); renderViewport(); scheduleSharpViewport("cursor-follow"); }
function showTouchCursor() { touchCursorVisible=shouldShowCursorOverlay({pointerMode, ...cursorReconciler}); renderTouchCursor(); }
function resetTouchCursor() { clearCursorSyncTimer(); touchCursorVisible=false; gestures.reset(); cursorReconciler.reset(); renderTouchCursor(); }
function mediaViewCrop(message) {
  const desktopWidth = Number(message?.desktopWidth) || remoteDesktop.width;
  const desktopHeight = Number(message?.desktopHeight) || remoteDesktop.height;
  const width = Number(message?.sourceWidth) || desktopWidth;
  const height = Number(message?.sourceHeight) || desktopHeight;
  return { enabled: message?.state !== "full" && (width < desktopWidth || height < desktopHeight),
    x: Number(message?.sourceX) || 0, y: Number(message?.sourceY) || 0, width, height,
    desktopWidth, desktopHeight, revision: Number(message?.revision) || 0,
    generation: Number(message?.generation) || 0, source: message };
}
function viewportHealthSnapshot() {
  const crop = renderedSharpView || activeViewportCrop();
  const gain = cropGain(crop, lastDecodedDimensions?.width || 1920, lastDecodedDimensions?.height || 1080);
  return { state: viewportTelemetry.state, revision: viewportTelemetry.revision, generation: viewportTelemetry.generation,
    crop: viewportTelemetry.crop, zoom: Number(viewport.zoom.toFixed(2)), pointer: pointerMode,
    pending: viewportPending?.revision || null,
    transitionMs: viewportPending ? Math.round(performance.now() - viewportPending.sentAt) : 0,
    gainX: Number(gain.x.toFixed(2)), gainY: Number(gain.y.toFixed(2)), requested: viewportLast };
}
function activeViewportCrop() {
  return viewportState.source ? mediaViewCrop(viewportState.source) : fullDesktopCrop(remoteDesktop.width, remoteDesktop.height);
}
function computeDesiredViewport() {
  if (streamModeValue() === "game" || !sharpZoomEnabled || sharpDisabledForSession) return fullDesktopCrop(remoteDesktop.width, remoteDesktop.height);
  return desiredSharpCrop({ viewport: viewport.snapshot(), viewportWidth: innerWidth, viewportHeight: innerHeight,
    content: viewport.content, desktopWidth: remoteDesktop.width, desktopHeight: remoteDesktop.height,
    wasEnabled: viewportState.enabled || !!viewportPending?.requested?.enabled });
}
function q16(value, extent) { return Math.max(0, Math.min(65535, Math.round(value / Math.max(1, extent) * 65535))); }
function armViewportTransitionTimeout(revision) {
  clearTimeout(viewportTransitionTimer);
  viewportTransitionTimer = setTimeout(() => {
    if (!viewportPending || viewportPending.revision !== revision) return;
    if (!viewportPending.recoveryRequested) {
      viewportPending.recoveryRequested = true;
      beginWssRecovery("viewport-transition-timeout");
      armViewportTransitionTimeout(revision);
      return;
    }
    viewerLog("warn", "viewport-transition-failed", { revision, requested: viewportPending.requested });
    viewportConsecutiveTimeouts++;
    viewportPending = null;
    if (viewportConsecutiveTimeouts >= 2) sharpDisabledForSession = true;
    sendViewportRequest(fullDesktopCrop(remoteDesktop.width, remoteDesktop.height), "viewport-timeout-full", true);
  }, 1_500);
}
function sendViewportRequest(crop, reason, force = false) {
  // The crop travels out of band (media-control viewport / media-view); frames carry only a generation.
  if (!usingWssMedia || !wssStartedHost || viewportPending) return false;
  const active = activeViewportCrop();
  if (!force && !cropNeedsUpdate(active, crop, remoteDesktop.width, remoteDesktop.height)) return false;
  const revision = viewportRevision = (viewportRevision + 1) >>> 0;
  viewportLastSentAt = performance.now();
  viewportLast = { enabled: crop.enabled, x: crop.x, y: crop.y, width: crop.width, height: crop.height };
  viewportPending = { revision, requested: crop, reason, sentAt: viewportLastSentAt, recoveryRequested: false, generation: 0, source: null };
  viewportTelemetry = { state: "refining", revision, generation: 0, crop: `${crop.width}x${crop.height}@${crop.x},${crop.y}` };
  sendControl({ type: "media-control", action: "viewport", protocol: WSS_PROTOCOL, epoch: mediaEpoch,
    revision, enabled: crop.enabled, xQ16: q16(crop.x, remoteDesktop.width), yQ16: q16(crop.y, remoteDesktop.height),
    widthQ16: q16(crop.width, remoteDesktop.width), heightQ16: q16(crop.height, remoteDesktop.height), reason });
  armViewportTransitionTimeout(revision);
  return true;
}
function flushSharpViewport(reason = "sharp-viewport") {
  viewportTimer = null;
  viewportDesired = computeDesiredViewport();
  if (viewportPending) return;
  sendViewportRequest(viewportDesired, reason);
}
function scheduleSharpViewport(reason = "sharp-viewport") {
  if (!usingWssMedia || !wssStartedHost) return;
  viewportDesired = computeDesiredViewport();
  if (viewportPending) return;
  clearTimeout(viewportTimer);
  const wait = Math.max(0, 100 - (performance.now() - viewportLastSentAt));
  viewportTimer = setTimeout(() => flushSharpViewport(reason), wait);
}
function handleMediaView(message) {
  if (!Number.isInteger(message.revision)) return;
  const minimumRevision = Math.max(viewportState.revision || 0, viewportPending?.revision || 0);
  if (message.revision < minimumRevision) return;
  viewportTelemetry = { state: message.state || "?", revision: message.revision, generation: message.generation || 0,
    crop: `${message.sourceWidth || "?"}x${message.sourceHeight || "?"}@${message.sourceX || 0},${message.sourceY || 0}` };
  if (message.state === "pending") {
    if (viewportPending?.revision === message.revision) Object.assign(viewportPending, { source: message, generation: message.generation || 0 });
    return;
  }
  const crop = mediaViewCrop(message);
  const completed = viewportPending?.revision === message.revision ? viewportPending : null;
  viewportState = { enabled: crop.enabled, revision: message.revision, generation: message.generation || 0, source: message };
  if (completed) {
    viewportPending = null;
    if (crop.enabled || completed.reason !== "viewport-timeout-full") viewportConsecutiveTimeouts = 0;
    clearTimeout(viewportTransitionTimer); viewportTransitionTimer = null;
  }
  const gain = cropGain(crop, lastDecodedDimensions?.width || 1920, lastDecodedDimensions?.height || 1080);
  viewportTelemetry = { state: crop.enabled ? `sharp ${gain.x.toFixed(1)}x/${gain.y.toFixed(1)}x` : "full",
    revision: message.revision, generation: message.generation || 0,
    crop: `${crop.width}x${crop.height}@${crop.x},${crop.y}` };
  scheduleSharpViewport("viewport-committed");
}
function viewportForGeneration(generation) {
  if (viewportPending?.source && viewportPending.generation === generation) return mediaViewCrop(viewportPending.source);
  if (viewportState.source && viewportState.generation === generation) return mediaViewCrop(viewportState.source);
  if (!viewportPending && viewportState.source) return { ...mediaViewCrop(viewportState.source), generation };
  if (!viewportPending && !viewportState.enabled) return { ...fullDesktopCrop(remoteDesktop.width, remoteDesktop.height), generation };
  return null;
}
function layoutSharpCanvas(crop) {
  if (!crop?.enabled) { sharpScreen.classList.remove("active"); renderedSharpView = null; return; }
  const content = viewport.content;
  const desktopWidth = Math.max(1, crop.desktopWidth || remoteDesktop.width);
  const desktopHeight = Math.max(1, crop.desktopHeight || remoteDesktop.height);
  // The stage is laid out inside #visual; content is in window coordinates.
  sharpScreen.style.left = `${content.x - visual.offsetLeft + crop.x / desktopWidth * content.width}px`;
  sharpScreen.style.top = `${content.y - visual.offsetTop + crop.y / desktopHeight * content.height}px`;
  sharpScreen.style.width = `${crop.width / desktopWidth * content.width}px`;
  sharpScreen.style.height = `${crop.height / desktopHeight * content.height}px`;
  sharpScreen.classList.add("active");
  renderedSharpView = crop;
}
function renderViewport() { const s = viewport.snapshot(); visual.style.transformOrigin = `${-visual.offsetLeft}px ${-visual.offsetTop}px`; visual.style.transform = `translate(${s.x}px,${s.y}px) scale(${s.zoom})`; hudState.zoom = s.zoom.toFixed(2); renderZoomChip(s.zoom); if (renderedSharpView) layoutSharpCanvas(renderedSharpView); renderTouchCursor(); }
function setZoom(next) { syncViewport(); const anchor=mobilePointerDevice && (pointerMode !== "desktop" || !cursorReconciler.synced) ? {x:innerWidth/2,y:innerHeight/2} : remoteCursor.screenPoint(viewport); viewport.setZoom(next, anchor); centerCameraOnCursor(); renderViewport(); scheduleSharpViewport("zoom"); }
function panBy(x,y) { syncViewport(); viewport.pan(x,y); renderViewport(); scheduleSharpViewport("pan"); }
addEventListener("resize", syncViewportAndFollow);
function setTouchButtons(mask) { touchButtons=mask; if (inputReady) input.buttons(mask); }
function setTouchButtonsAtCursor(mask) { const point=remoteCursor.absolutePoint(); touchButtons=mask; if (inputReady && input.buttonsAt(point.x,point.y,mask)) cursorReconciler.noteSent(input.lastCursorSequence); updatePointerHud(); }
function dispatchGesture(actions) {
  if (!desktopAvailable || desktopAwaitingFrame) return;
  for (const action of actions) {
    if (action.type === "suppress-motion") { input.clearMotion(); if (motionFrame !== null) cancelAnimationFrame(motionFrame); motionFrame = null; }
    else if (action.type === "motion") { const remote=remoteCursor.remoteDeltaForScreen(action.dx,action.dy,viewport,remoteDesktop); if (pointerMode === "desktop") { if (!cursorReconciler.synced) { requestCursorSync(); continue; } updateRemoteCursor(cursor => cursor.moveScreen(action.dx,action.dy,viewport)); showTouchCursor(); const point=remoteCursor.absolutePoint(); if (inputReady) queueTouchAbsolute(point.x,point.y); } else if (inputReady) queueTouchMotion(remote.dx,remote.dy); }
    else if (action.type === "motion-end" && pointerMode === "desktop" && cursorReconciler.synced) setTouchButtonsAtCursor(touchButtons);
    else if (action.type === "tap" && inputReady) { if (pointerMode === "desktop") { if (!cursorReconciler.synced) { requestCursorSync(); continue; } setTouchButtonsAtCursor(1); setTouchButtons(0); } else { setTouchButtons(1); setTouchButtons(0); } }
    else if (action.type === "drag-start") { if (pointerMode === "desktop" && cursorReconciler.synced) setTouchButtonsAtCursor(1); else if (pointerMode === "game") setTouchButtons(1); }
    else if (action.type === "drag-end" && touchButtons) { if (pointerMode === "desktop" && cursorReconciler.synced) setTouchButtonsAtCursor(0); else setTouchButtons(0); }
    else if (action.type === "right-tap" && inputReady) { if (pointerMode === "desktop") { if (!cursorReconciler.synced) { requestCursorSync(); continue; } setTouchButtonsAtCursor(2); setTouchButtons(0); } else { setTouchButtons(2); setTouchButtons(0); } }
    else if (action.type === "pinch") setZoom(viewport.zoom * action.scale);
    else if (action.type === "translate") { if (viewport.zoom === 1) { if (inputReady) { const wheel = touchTranslateWheel(action); queueTouchMotion(0, 0, wheel.x, wheel.y); } } else panBy(action.dx,action.dy); }
  }
  hudState.touch = gestures.cancelled ? "cancelled" : (gestures.dragActive ? "drag" : gestures.mode || (gestures.points.size > 1 ? "two-finger" : gestures.points.size ? "touch" : "…"));
}
// A desktop-mode mouse drag keeps the pointer captured: moves over the strip
// or outside the browser window still reach the mousemove handler (clamped
// to the picture's edge, so the remote cursor reaches the monitor edge for
// edge hold), and the release is never lost.
inputSurface.addEventListener("pointerdown", e => { if (e.pointerType === "mouse" && pointerMode === "desktop") { try { inputSurface.setPointerCapture(e.pointerId); } catch {} } if (e.pointerType === "pen") lastNonMousePointerAt = performance.now(); if (e.pointerType !== "mouse") mousePointerActive = false; if (e.pointerType !== "touch") return; lastNonMousePointerAt = performance.now(); showTouchCursor(); inputSurface.setPointerCapture(e.pointerId); dispatchGesture(gestures.down(e.pointerId,{x:e.clientX,y:e.clientY},e.timeStamp)); e.preventDefault(); });
inputSurface.addEventListener("pointermove", e => { if (e.pointerType !== "touch") return; dispatchGesture(gestures.move(e.pointerId,{x:e.clientX,y:e.clientY})); e.preventDefault(); });
inputSurface.addEventListener("pointerup", e => { if (e.pointerType !== "touch") return; dispatchGesture(gestures.up(e.pointerId,e.timeStamp)); e.preventDefault(); });
inputSurface.addEventListener("pointercancel", e => { if (e.pointerType !== "touch") return; dispatchGesture(gestures.cancel(e.pointerId)); releaseInput(); resetTouchCursor(); hudState.touch = "cancelled"; e.preventDefault(); });

// ---- session chrome: strip, popovers, settings, fullscreen handle ---------
// UI only: every action below calls the existing input, monitor, quality and
// session functions. See chrome.js for the popover and edge-reveal mechanics.
const sessionStrip = document.getElementById("session-strip");
const stripHandle = document.getElementById("strip-handle");
const stripSettingsButton = document.getElementById("strip-settings");
const popovers = createPopoverController({
  scrim: document.getElementById("popover-scrim"),
  surface: inputSurface,
  layout: chromeLayout,
  onChange(opened, closed) {
    if (opened === "pop-monitors") renderMonitorList();
    if (opened === "pop-shortcuts") renderShortcutList();
    if (opened === "pop-files") renderFilesSlot();
    if (closed === "pop-arrows") stopPadRepeat();
    if (!opened) settleStripPeek();
  },
});
function openPanel(id, anchor, viaKeyboard) { popovers.open(id, document.getElementById(id), anchor, viaKeyboard); }

document.getElementById("capture").onclick = () => {
  popovers.close();
  if (locked) { document.exitPointerLock?.(); return; }
  // Capturing implies Game mode (relative motion); Settings › Input returns to Work.
  if (pointerMode !== "game") setPointerMode("game");
  requestPointerLock();
};
pointerModeSelect.value = pointerMode;
pointerModeSelect.onchange = () => setPointerMode(pointerModeSelect.value);
qualitySelect.value = qualityPreset;
qualitySelect.onchange = e => {
  e.stopPropagation();
  qualityPreset = qualitySelect.value;
  settings.quality = qualityPreset;
  persistSettings();
  if (signalingRegistered) {
    const effectiveQuality = qualityPreset === "auto" ? "high" : qualityPreset;
    sendControl({ type: "media-control", action: "quality", protocol: WSS_PROTOCOL,
      ...(usingWssMedia ? { epoch: mediaEpoch } : {}),
      codec: codecSession.codec, streamMode: streamModeValue(), reason: "user-preset",
      quality: effectiveQuality });
  }
};
const keyboardLockSwitch = document.getElementById("setting-keyboard-lock");
keyboardLockSwitch.checked = settings.keyboardLock;
if (!navigator.keyboard?.lock) {
  keyboardLockSwitch.disabled = true;
  keyboardLockSwitch.closest(".setting-row").querySelector(".setting-help").textContent = "This browser cannot capture system shortcuts; use Shortcuts in the strip.";
}
keyboardLockSwitch.onchange = () => { settings.keyboardLock = keyboardLockSwitch.checked; persistSettings(); updateKeyboardLock(); };
const clipboardAutoCopySwitch = document.getElementById("setting-clipboard-auto-copy");
clipboardAutoCopySwitch.checked = settings.clipboardAutoCopy;
clipboardAutoCopySwitch.onchange = () => {
  settings.clipboardAutoCopy = clipboardAutoCopySwitch.checked;
  persistSettings();
  remoteClipboardWriter.setAutomatic(settings.clipboardAutoCopy, document.hasFocus() && desktopKind !== "secure");
};
const scrollSpeedSelect = document.getElementById("setting-scroll-speed");
scrollSpeedSelect.value = String(settings.scrollSpeed);
scrollSpeedSelect.onchange = () => { settings.scrollSpeed = Number(scrollSpeedSelect.value); persistSettings(); };
const defaultMonitorSelect = document.getElementById("setting-default-monitor");
defaultMonitorSelect.onchange = () => { settings.defaultMonitor = defaultMonitorSelect.value; persistSettings(); renderMonitorChrome(); };
// A switch: toggling keeps the Monitors popover open (rows above close it).
document.getElementById("follow-window").onclick = () => toggleFollowWindow();

function renderZoomChip(zoom) {
  const chip = document.getElementById("zoom-reset");
  if (!chip) return;
  const zoomed = Math.abs(zoom - 1) > 0.01;
  if (chip.hidden === zoomed) chip.hidden = !zoomed;
  if (!zoomed) return;
  chip.textContent = `${zoom.toFixed(1)}× · Reset`;
  chip.setAttribute("aria-label", `Reset zoom from ${zoom.toFixed(1)}×`);
}
document.getElementById("zoom-reset").onclick = e => { e.stopPropagation(); setZoom(1); inputSurface.focus({ preventScroll: true }); };

function toggleFullscreen(e) { e?.stopPropagation(); if (document.fullscreenElement) document.exitFullscreen?.(); else document.documentElement.requestFullscreen?.(); }
document.addEventListener("fullscreenchange", () => {
  const full = Boolean(document.fullscreenElement);
  document.body.classList.toggle("is-fullscreen", full);
  const button = document.getElementById("strip-fullscreen");
  button.setAttribute("aria-pressed", String(full));
  button.setAttribute("aria-label", full ? "Exit fullscreen" : "Enter fullscreen");
  button.title = full ? "Exit fullscreen" : "Fullscreen";
  document.getElementById("more-fullscreen-state").textContent = full ? "On" : "Off";
  setStripPeek(false);
  popovers.reposition();
  requestAnimationFrame(syncViewportAndFollow);
});

// Fullscreen (desktop layout): the strip is hidden. Resting the pointer on the
// top edge for 300 ms shows a handle; the handle brings the strip back as an
// overlay that stays while the pointer is on it or a popover is open.
let stripPeek = false;
let stripPeekTimer = null;
const edgeReveal = createEdgeReveal({ delayMs: 300, edgePx: 4, onArm: () => { stripHandle.hidden = false; } });
function stripHiddenByFullscreen() { return document.body.classList.contains("is-fullscreen") && chromeLayout() === "desktop"; }
function setStripPeek(on) {
  clearTimeout(stripPeekTimer);
  stripPeek = Boolean(on) && stripHiddenByFullscreen();
  document.body.classList.toggle("strip-peek", stripPeek);
  stripHandle.hidden = true;
  edgeReveal.reset();
}
function settleStripPeek() {
  if (!stripPeek) return;
  clearTimeout(stripPeekTimer);
  stripPeekTimer = setTimeout(() => {
    if (!popovers.current && !sessionStrip.matches(":hover")) setStripPeek(false);
  }, 300);
}
document.addEventListener("mousemove", e => {
  if (!stripHiddenByFullscreen() || locked || stripPeek) return;
  edgeReveal.move(e.clientY, true);
  if (!stripHandle.hidden && e.clientY > 28 && e.target !== stripHandle && !stripHandle.contains(e.target)) {
    stripHandle.hidden = true;
    edgeReveal.reset();
  }
}, { passive: true });
stripHandle.addEventListener("pointerenter", e => { if (e.pointerType === "mouse") setStripPeek(true); });
stripHandle.addEventListener("click", () => setStripPeek(true));
stripHandle.addEventListener("mousedown", e => e.preventDefault());
sessionStrip.addEventListener("pointerenter", () => clearTimeout(stripPeekTimer));
sessionStrip.addEventListener("pointerleave", () => { if (stripPeek && !popovers.current) settleStripPeek(); });

// Touch keyboard: a hidden text field raises the soft keyboard; its input is
// mirrored to the remote by the reconciliation listeners above.
const keyboardButton = document.querySelector('[data-ui-action="keyboard"]');
function renderKeyboardButton() {
  const shown = document.activeElement === keyboardInput;
  keyboardButton.setAttribute("aria-pressed", String(shown));
  keyboardButton.setAttribute("aria-label", shown ? "Hide keyboard" : "Show keyboard");
}
function toggleTouchKeyboard() {
  popovers.close({ restore: false });
  if (document.activeElement === keyboardInput) { keyboardInput.blur(); resetKeyboardDraft(); releaseLatchedModifiers(); }
  else keyboardInput.focus({ preventScroll: true });
  renderKeyboardButton();
}
keyboardInput.addEventListener("focus", renderKeyboardButton);
keyboardInput.addEventListener("blur", renderKeyboardButton);

const stripActions = {
  keyboard: () => toggleTouchKeyboard(),
  monitors: (button, viaKeyboard) => openPanel("pop-monitors", button, viaKeyboard),
  pointer: () => { popovers.close(); setPointerMode(pointerMode === "desktop" ? "game" : "desktop"); },
  shortcuts: (button, viaKeyboard) => openPanel("pop-shortcuts", button, viaKeyboard),
  keys: (button, viaKeyboard) => openPanel("pop-keys", button, viaKeyboard),
  arrows: (button, viaKeyboard) => openPanel("pop-arrows", button, viaKeyboard),
  files: (button, viaKeyboard) => openPanel("pop-files", button, viaKeyboard),
  more: (button, viaKeyboard) => openPanel("pop-more", button, viaKeyboard),
  settings: (_button, viaKeyboard) => openPanel("settings-panel", stripSettingsButton, viaKeyboard),
  sound: () => soundController.toggle(),
  fullscreen: () => { popovers.close(); toggleFullscreen(); },
  feedback: () => { popovers.close(); globalThis.BPFeedback?.open(); },
};
// Local PC sound button: shown only with the host's audio capability; the muted
// glyph shows while muted or blocked (soundButtonView).
const stripSound = document.getElementById("strip-sound");
function renderSoundSlot() {
  const view = soundButtonView({ capable: hostAudioCapable, state: soundController.state });
  stripSound.hidden = view.hidden;
  stripSound.setAttribute("aria-pressed", String(view.pressed));
  stripSound.setAttribute("aria-label", view.label);
  stripSound.title = view.label;
  stripSound.classList.toggle("sound-muted", view.silent);
}
document.querySelectorAll("[data-ui-action]").forEach(button => {
  // A pointer press must not move keyboard focus off the remote input surface.
  button.addEventListener("mousedown", e => e.preventDefault());
  button.addEventListener("click", e => {
    e.stopPropagation();
    stripActions[button.dataset.uiAction]?.(button, e.detail === 0);
  });
});
// Popover keys and rows keep focus (and a raised soft keyboard) where it was.
document.querySelectorAll(".bp-popover").forEach(panel => panel.addEventListener("mousedown", e => {
  if (e.target.closest("button")) e.preventDefault();
}));
for (const id of ["remote-clipboard", "zoom-reset", "connection-retry"])
  document.getElementById(id).addEventListener("mousedown", e => e.preventDefault());
document.getElementById("close-settings").onclick = () => popovers.close();
addEventListener("resize", () => popovers.reposition());
touchLayoutQuery.addEventListener?.("change", () => { popovers.close({ restore: false }); setStripPeek(false); renderShortcutList(); });

// Files: the strip icon exists only with the host's fileTransfer capability.
// file-ui.js renders the offer and transfer rows inside the popover; this
// keeps the icon's attention dot, the picker and the empty state in step.
const stripFiles = document.getElementById("strip-files");
const fileChoose = document.getElementById("file-choose");
const filePicker = document.getElementById("file-picker");
const filesStatus = document.getElementById("files-status");
function renderFilesSlot() {
  const capable = fileUi.enabled;
  stripFiles.hidden = !capable;
  if (!capable) {
    if (popovers.isOpen("pop-files")) popovers.close({ restore: false });
    return;
  }
  const rows = fileClient.transfers;
  const running = rows.filter(row => !["done", "failed", "cancelled"].includes(row.state)).length;
  const offerShown = !document.querySelector("#file-transfer [data-file-offer]").hidden;
  const badge = stripFiles.querySelector(".strip-badge");
  badge.hidden = !(running || offerShown);
  stripFiles.setAttribute("aria-label", offerShown ? "Files, the PC is offering files" : running ? `Files, ${running} ${running === 1 ? "transfer" : "transfers"} running` : "Files");
  const available = fileTransferAvailable();
  fileChoose.disabled = !available;
  filesStatus.textContent = !available
    ? (desktopKind === "secure" ? "Files are paused on the Windows secure desktop." : "Files are available once remote input is ready.")
    : rows.length || offerShown ? "" : (chromeLayout() === "touch" ? "Send files to the PC from this device." : "Choose files, or drop or paste them onto the stream.");
}
fileClient.addEventListener("change", () => renderFilesSlot());
// Dismissing the offer re-renders file-ui without a client change event.
document.getElementById("file-transfer").addEventListener("click", () => queueMicrotask(renderFilesSlot), true);
fileChoose.onclick = () => {
  if (!fileTransferAvailable()) { renderFilesSlot(); return; }
  filePicker.click();
};
filePicker.onchange = () => {
  const files = Array.from(filePicker.files || []);
  filePicker.value = "";
  if (files.length && fileTransferAvailable()) fileClient.upload(files, "save");
  inputSurface.focus({ preventScroll: true });
};

const aboutState = { relay: null };
function renderAbout(update = {}) {
  Object.assign(aboutState, update);
  document.getElementById("about-relay").textContent = aboutState.relay || "Not reported";
  document.getElementById("about-host").textContent = "Not reported by this host";
  document.getElementById("about-receiver").textContent = clientBuild;
}

let diagnosticsVisible = diagnosticsEnabled;
function diagnosticsReport() {
  const view = viewportHealthSnapshot();
  return [
    "TRANSPORT",
    `state       ${connectionStatusDetail}`,
    `route       ${hudState.route}`,
    `rtt/jitter  ${hudState.rtt} / ${hudState.jit}`,
    `loss        ${hudState.loss}`,
    "",
    "VIDEO",
    `decoder     ${hudState.dec}`,
    `dimensions  ${hudState.dimensions}`,
    `fps         ${hudState.fps}`,
    `queue/drop  ${hudState.queued} / ${hudState.dropped}`,
    `presented   ${hudState.presented}`,
    `WSS direct  ${framePresentationTelemetry.immediatePresented}/${framePresentationTelemetry.immediateAttempts}`,
    `RAF merged  ${framePresentationTelemetry.rafCoalesced}`,
    "",
    "INPUT",
    `mode        ${pointerMode === "desktop" ? "work" : "game"}`,
    `capture     ${locked ? "captured" : "released"}`,
    `control     ${hudState.control}`,
    `cursor      ${cursorAppearance.kind} / ${inputSurface.style.cursor} (${cursorAppearance.accepted}/${cursorAppearance.received} updates)`,
    `host apply  ${latestInputStatus ? `${latestInputStatus.appliedEvents}/${latestInputStatus.requestedEvents} (${latestInputStatus.failedPackets} failed)` : "…"}`,
    "",
    "VIEWPORT",
    `zoom        ${view.zoom}x`,
    `state       ${view.state}`,
    `crop        ${view.crop}`,
    `generation  ${view.generation}`,
  ].join("\n");
}
// ?diag=1 shows the live HUD over the stream; Settings › Diagnostics shows the report.
function setDiagnosticsVisible(visible) {
  diagnosticsVisible = Boolean(visible);
  if (diagnosticsOutput) {
    diagnosticsOutput.hidden = !diagnosticsVisible;
    diagnosticsOutput.textContent = diagnosticsVisible ? diagnosticsReport() : "Diagnostics are hidden.";
  }
  const button = document.getElementById("open-diagnostics");
  if (button) button.textContent = diagnosticsVisible ? "Hide" : "Show";
  if (diagnosticsVisible) document.getElementById("diagnostics-details").open = true;
}
document.body.classList.toggle("diagnostics-mode", diagnosticsEnabled);
document.getElementById("open-diagnostics").onclick = e => { e.stopPropagation(); setDiagnosticsVisible(!diagnosticsVisible); };
document.getElementById("copy-diagnostics").onclick = async e => {
  e.stopPropagation();
  const button = e.currentTarget;
  setDiagnosticsVisible(true);
  try {
    clearRemoteClipboard();
    await navigator.clipboard?.writeText(diagnosticsReport());
    button.textContent = "Copied";
    setTimeout(() => { button.textContent = "Copy report"; }, 1400);
  } catch { button.textContent = "Copy unavailable"; }
};
document.getElementById("dismiss-context-hint").onclick = e => {
  e.stopPropagation();
  const mode = contextHint.dataset.mode === "game" ? "game" : "work";
  try { localStorage.setItem(`bp_hint_${mode}_dismissed`, "1"); } catch {}
  contextHint.hidden = true;
  inputSurface.focus({ preventScroll: true });
};

document.getElementById("connection-retry").onclick = e => { e.stopPropagation(); terminalSignalingFailure = false; restartPeer("user-retry", true); };
firstFrameRetry.onclick = e => { e.stopPropagation(); startFirstFrameHandshake("user-retry", true); };
secureDeny.onclick = e => {
  e.stopPropagation();
  if (secureDenyState.pending || !secureCapabilities.includes("deny")) return;
  if (!sendControl({ type: "secure-action", action: "deny" })) return;
  secureDenyState = { pending: true, error: "" };
  renderDesktopState();
  // The host answers with secure-action-result; don't leave the button stuck if it never does.
  setTimeout(() => {
    if (!secureDenyState.pending) return;
    secureDenyState = { pending: false, error: "no answer from the PC" };
    renderDesktopState();
  }, 8000);
};
setPointerMode(pointerMode,false);
setDiagnosticsVisible(diagnosticsEnabled);
renderAbout();
renderMonitorList();
renderShortcutList();
renderKeyboardModifiers();
renderKeyboardButton();

function clamp(v, lo, hi) { return Math.max(lo, Math.min(hi, v)); }
function clamp16(v) { return clamp(v, -32768, 32767); }
