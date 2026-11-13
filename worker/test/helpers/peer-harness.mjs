// The real peer (public/peer.js) on a fake RTCPeerConnection, with every callback to the receiver
// recorded in `calls` in the order it ran.
import { createPeer } from "../../public/peer.js";
import { createSessionState } from "../../public/session-state.js";
import { createCodecSession, TRANSPORT } from "../../public/transport.js";

const HEVC_CODEC = { mimeType: "video/H265", clockRate: 90000, sdpFmtpLine: "profile-id=1" };
const H264_CODEC = { mimeType: "video/H264", clockRate: 90000, sdpFmtpLine: "packetization-mode=1" };

export class FakePeerConnection {
  constructor(options) {
    this.options = options;
    this.transceivers = [];
    this.channels = [];
    this.closed = false;
    this.iceConnectionState = "new";
    this.connectionState = "new";
    this.offerError = null;
  }
  addTransceiver(kind, init) {
    const transceiver = { kind, init, receiver: { jitterBufferTarget: undefined }, setCodecPreferences(codecs) { this.preferences = codecs; } };
    this.transceivers.push(transceiver);
    return transceiver;
  }
  createDataChannel(label, options) {
    const channel = { label, options, closed: false, close() { this.closed = true; } };
    this.channels.push(channel);
    return channel;
  }
  async createOffer() { if (this.offerError) throw this.offerError; return { sdp: "offer-sdp" }; }
  async setLocalDescription() { }
  close() { this.closed = true; }
  getStats() { return new Map(); }
}

export function peerHarness({ transport = TRANSPORT.AUTO, decoderPath = "native", env: overrides = {}, state: fields = {}, controlSent = true } = {}) {
  const calls = [];
  const pcs = [];
  const state = Object.assign(createSessionState({ wssSelected: transport === TRANSPORT.WSS }), {
    ws: { readyState: 1 },
    authenticatedSession: { controlReady: true, mediaReady: false, resetPeer: () => calls.push(["peer-reset"]), close() {} },
  }, fields);
  const log = name => (...args) => { calls.push([name, ...args]); };
  const env = {
    state,
    hudState: {},
    requestedTransport: transport,
    iceTransportPolicy: "all",
    useWebCodecs: decoderPath === "webcodecs",
    codecSession: createCodecSession({ transport: transport === TRANSPORT.WSS ? "wss" : "webrtc", decoderPath }),
    mobilePointerDevice: false,
    isCurrentPeer: (generation, candidate = state.pc) => generation === state.peerGeneration && candidate === state.pc,
    sendControl: message => { calls.push(["send-control", message.type]); return controlSent; },
    startupSnapshot: () => ({ framesReceived: 0, nativeFramesPresented: 0 }),
    updateInputReady: log("input-ready"),
    handleCursorState: log("cursor"),
    handleConnectionState: log("connection-state"),
    onTrack: log("track"),
    startFirstFrameHandshake: log("first-frame"),
    switchToWss: log("switch-to-wss"),
    onRtcHealthy: log("rtc-healthy"),
    requestPointerLock: log("pointer-lock"),
    markBad: log("mark-bad"),
    markGood: log("mark-good"),
    releaseInput: log("release-input"),
    recycleSignaling: log("recycle-signaling"),
    endRtcPeerOnly: log("end-rtc-peer-only"),
    reconnectSignaling: log("reconnect-signaling"),
    requestWssMediaStart: log("start-wss"),
    invalidatePendingMonitorRequest: log("invalidate-monitor"),
    resetFirstFrameHandshake: log("reset-handshake"),
    renderDesktopState: log("render-desktop"),
    clearHostQuality: log("clear-host-quality"),
    resetFileDropPreview: log("reset-file-drop"),
    stopWssMedia: log("stop-wss"),
    detachRtcSound: log("detach-sound"),
    clearWssTimers: log("clear-wss-timers"),
    resetInputUi: log("reset-input-ui"),
    releaseMediaPipeline: log("release-media"),
    viewerLog: (level, event) => calls.push(["log", level, event]),
    createPeerConnection: options => { const pc = new FakePeerConnection(options); pcs.push(pc); return pc; },
    receiverCapabilities: () => ({ codecs: [HEVC_CODEC, H264_CODEC] }),
    ...overrides,
  };
  const peer = createPeer(env);
  return { peer, env, state, calls, pcs, names: () => calls.map(call => call[0]) };
}
