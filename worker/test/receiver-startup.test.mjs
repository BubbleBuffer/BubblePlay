import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";
import { createRtcStartupFallback, shouldFallbackToWssOnMissingRtp } from "../public/media.js";
import { rememberedRtcFailure, rememberRtcFailure, rtcFailureStorage, RTC_FAILURE_STORAGE_KEY } from "../public/transport.js";
import { rtcPeerEndAction } from "../public/recovery.js";

const source = readFileSync(new URL("../public/receiver.js", import.meta.url), "utf8");

function probeHarness() {
  let now = 0;
  let nextTimer = 0;
  const timers = new Map();
  const state = { connected: true, peerConnected: true, framesReceived: 0, nativeFramesPresented: 0 };
  const result = { fallbacks: 0, healthy: 0, statsCalls: 0 };
  let current = true;
  let stats = async () => new Map();
  const probe = createRtcStartupFallback({
    now: () => now,
    schedule: (fn, delay) => { const id = ++nextTimer; timers.set(id, { fn, due: now + delay }); return id; },
    cancel: id => timers.delete(id),
    isCurrent: () => current,
    snapshot: () => ({ ...state }),
    getStats: () => { result.statsCalls++; return stats(); },
    onFallback: () => result.fallbacks++,
    onHealthy: () => result.healthy++,
  });
  return { probe, state, result, timers,
    setStats: value => { stats = value; },
    invalidate: () => { current = false; },
    async advance(to) {
      for (;;) {
        const entry = [...timers].sort((a, b) => a[1].due - b[1].due).find(([, timer]) => timer.due <= to);
        if (!entry) break;
        const [id, timer] = entry;
        now = timer.due; timers.delete(id); timer.fn();
        for (let i = 0; i < 4; i++) await Promise.resolve();
      }
      now = to;
      for (let i = 0; i < 4; i++) await Promise.resolve();
    },
  };
}

test("startup constructs signaling immediately; socket reconnect retains its 250ms delay", async () => {
  const sockets = [];
  const timers = [];
  class Socket {
    static OPEN = 1; static CONNECTING = 0;
    constructor(url) { this.url = url; this.readyState = 0; sockets.push(this); }
  }
  const context = vm.createContext({ WebSocket: Socket, URL,
    inputStatusReason: "admin-required", latestInputStatus: { failedPackets: 2 },
    terminalSignalingFailure: false, reconnectTimer: null, ws: null,
    signalingGeneration: 0, signalingRegistered: false, authenticatedSession: null,
    hudState: {}, hud: { classList: { add() {} } },
    signalingSocketUrl: () => "wss://test.invalid/ws", debugTrace() {}, recordSignalingError() {},
    shouldReconnectAfterClose: () => true, restartPeer() {}, cleanupPeer() {}, handleSignal() {},
    setTimeout: (fn, delay) => { timers.push({ fn, delay }); return timers.length; },
  });
  const fn = source.slice(source.indexOf("function reconnectSignaling("), source.indexOf("\nfunction signalingSocketUrl("));
  vm.runInContext(fn + '\nreconnectSignaling("startup");', context);
  assert.equal(sockets.length, 1);
  assert.equal(timers.length, 0);
  assert.equal(context.inputStatusReason, null);
  assert.equal(context.latestInputStatus, null);
  context.inputStatusReason = "admin-unavailable";
  sockets[0].readyState = 3;
  sockets[0].onclose({ code: 1006 });
  await Promise.resolve();
  await Promise.resolve();
  assert.equal(context.inputStatusReason, null);
  assert.equal(sockets.length, 1);
  assert.equal(timers[0].delay, 250);
  timers[0].fn();
  assert.equal(sockets.length, 2);
});

test("first-frame is requested directly after successful offer signaling", async () => {
  const events = [];
  const start = source.indexOf("    const offer = await candidate.createOffer();");
  const end = source.indexOf("  } catch (e)", start);
  const block = source.slice(start, end);
  const context = vm.createContext({ generation: 3,
    candidate: { createOffer: async () => ({ sdp: "offer-sdp" }), setLocalDescription: async () => events.push("local-description") },
    isCurrentPeer: () => true, rtcOfferSignaled: false,
    sendControl: message => { events.push(message.type); return true; },
    startFirstFrameHandshake: reason => events.push(`first-frame:${reason}`),
  });
  await vm.runInContext(`(async () => { ${block} })()`, context);
  assert.deepEqual(events, ["local-description", "offer", "first-frame:offer-sent"]);
  assert.equal(context.rtcOfferSignaled, true);
  events.length = 0; context.sendControl = () => false; context.rtcOfferSignaled = false;
  await vm.runInContext(`(async () => { ${block} })()`, context);
  assert.deepEqual(events, ["local-description"]);
  assert.equal(context.rtcOfferSignaled, false);
});

test("slow ICE renews an exhausted offer-time handshake; normal ICE retains its request", () => {
  const timers = new Map();
  let nextTimer = 0;
  let nextRequest = 0;
  const sent = [];
  const state = { requestId: null, phase: "idle", presented: false, timer: null };
  const candidate = { iceConnectionState: "connected" };
  const context = vm.createContext({ firstFrameHandshake: state,
    WebSocket: { OPEN: 1 }, ws: { readyState: 1 }, rtcOfferSignaled: false,
    usingWssMedia: false, mediaEpoch: "epoch", useWebCodecs: false, transitionNeedsKeyframe: false,
    desktopAvailable: true, desktopAwaitingFrame: false,
    clearFirstFrameWatchdog() { if (state.timer !== null) timers.delete(state.timer); state.timer = null; },
    firstFrameRequestId: () => `request-${++nextRequest}`,
    sendFirstFrameRequest: () => sent.push(state.requestId),
    renderDesktopState() {}, sendClientLog() {}, sendControl() {}, recoverWssStartup() {},
    recoverRtcInPlaceTransition() {}, consumeRtcTransitionRestart: () => false,
    WSS_PROTOCOL: 4, codecSession: { codec: "hevc" },
    setTimeout: (fn, delay) => { const id = ++nextTimer; timers.set(id, { fn, delay }); return id; },
    candidate, generation: 0, isCurrentPeer: () => true, hudState: {}, connected: false,
    rtcStartupFallback: { progress() {} }, hud: { classList: { remove() {} } },
    mobilePointerDevice: true,
  });
  const handshakeFns = source.slice(source.indexOf("function armFirstFrameWatchdog("), source.indexOf("function markFirstFramePresented("));
  const iceHandler = source.slice(source.indexOf("  candidate.oniceconnectionstatechange ="), source.indexOf("  candidate.ontrack ="));
  vm.runInContext(handshakeFns + '\n' + iceHandler, context);
  vm.runInContext('startFirstFrameHandshake("desktop-state");', context);
  assert.equal(nextRequest, 0, "request cannot precede the offer");
  context.rtcOfferSignaled = true;
  vm.runInContext('startFirstFrameHandshake("offer-sent");', context);
  const first = state.requestId;
  candidate.oniceconnectionstatechange();
  assert.equal(state.requestId, first); assert.equal(sent.length, 1);
  let timer = timers.get(state.timer); assert.equal(timer.delay, 5_000); timer.fn();
  assert.equal(state.requestId, first); assert.equal(sent.length, 2, "first watchdog retries same request");
  timer = timers.get(state.timer); assert.equal(timer.delay, 5_000); timer.fn();
  assert.equal(state.phase, "failed");
  candidate.oniceconnectionstatechange();
  assert.notEqual(state.requestId, first); assert.equal(state.phase, "starting");
  assert.equal(sent.length, 3, "late media readiness renews the handshake");
});

test("silent connected startup gets the full five-second first-frame budget", async () => {
  const h = probeHarness(); h.probe.requestSent();
  await h.advance(4_999);
  assert.equal(h.result.fallbacks, 0); assert.equal(h.result.statsCalls, 0);
  await h.advance(5_000);
  assert.equal(h.result.fallbacks, 1);
});

test("ICE and peer connecting cannot trigger no-RTP fallback even after five seconds", async () => {
  for (const key of ["connected", "peerConnected"]) {
    const h = probeHarness(); h.state[key] = false; h.probe.requestSent();
    await h.advance(12_000);
    assert.equal(h.result.fallbacks, 0); assert.equal(h.result.statsCalls, 0);
    h.state[key] = true; h.probe.progress();
    await h.advance(16_999); assert.equal(h.result.fallbacks, 0);
    await h.advance(17_000); assert.equal(h.result.fallbacks, 1);
  }
});

test("startup progress and watchdog retry each preserve another five seconds", async () => {
  const h = probeHarness(); h.probe.requestSent();
  await h.advance(4_000); h.probe.progress();
  await h.advance(5_000); h.probe.requestSent();
  await h.advance(9_999); assert.equal(h.result.fallbacks, 0);
  await h.advance(10_000); assert.equal(h.result.fallbacks, 1);
});

test("RTP arrival before decoded output keeps native startup on WebRTC", async () => {
  for (const counters of [{ packetsReceived: 1 }, { bytesReceived: 1200 }]) {
    const h = probeHarness();
    h.setStats(async () => new Map([["video", { type: "inbound-rtp", kind: "video", ...counters }]]));
    h.probe.requestSent(); await h.advance(5_000);
    assert.equal(h.result.fallbacks, 0); assert.equal(h.result.healthy, 1); assert.equal(h.timers.size, 0);
  }
});

test("native or encoded presentation counters stop startup fallback", async () => {
  for (const key of ["framesReceived", "nativeFramesPresented"]) {
    const h = probeHarness(); h.probe.requestSent(); h.state[key] = 1;
    await h.advance(5_000);
    assert.equal(h.result.fallbacks, 0); assert.equal(h.result.healthy, 1); assert.equal(h.result.statsCalls, 0);
  }
});

test("rejected stats are retried without interpreting them as absent RTP", async () => {
  const h = probeHarness(); h.setStats(async () => { throw new Error("temporary"); });
  h.probe.requestSent(); await h.advance(7_000);
  assert.equal(h.result.fallbacks, 0); assert.equal(h.result.statsCalls, 3);
  h.setStats(async () => new Map()); await h.advance(8_000);
  assert.equal(h.result.fallbacks, 1);
});

test("a video stats record without receive counters cannot prove absent RTP", async () => {
  const h = probeHarness();
  h.setStats(async () => new Map([["video", { type: "inbound-rtp", kind: "video" }]]));
  h.probe.requestSent(); await h.advance(8_000);
  assert.equal(h.result.fallbacks, 0);
});

test("stalled or stale stats cannot abandon a new peer or a retried request", async () => {
  for (const action of ["invalidate", "retry", "stop", "progress"]) {
    const h = probeHarness(); let resolve;
    h.setStats(() => new Promise(done => { resolve = done; }));
    h.probe.requestSent(); await h.advance(5_000);
    await h.advance(6_000); assert.equal(h.result.fallbacks, 0);
    if (action === "invalidate") h.invalidate();
    if (action === "retry") h.probe.requestSent();
    if (action === "stop") h.probe.stop();
    if (action === "progress") h.probe.progress();
    resolve(new Map()); await h.advance(6_000);
    assert.equal(h.result.fallbacks, 0);
  }
});

test("fallback rechecks current ICE and peer state after async stats", async () => {
  const h = probeHarness(); let resolve;
  h.setStats(() => new Promise(done => { resolve = done; }));
  h.probe.requestSent(); await h.advance(5_000);
  h.state.peerConnected = false; resolve(new Map()); await h.advance(5_000);
  assert.equal(h.result.fallbacks, 0);
});

test("fallback requires known stats, both connected states, and no recent progress", () => {
  const state = { connected: true, peerConnected: true, statsAvailable: true,
    requestAtMs: 1_000, lastProgressAtMs: 1_000, nowMs: 6_000 };
  assert.equal(shouldFallbackToWssOnMissingRtp(state), true);
  for (const override of [{ nowMs: 5_999 }, { lastProgressAtMs: 1_001 }, { peerConnected: false },
    { connected: false }, { statsAvailable: false }, { rtpBytesReceived: 1 }, { rtpPacketsReceived: 1 }]) {
    assert.equal(shouldFallbackToWssOnMissingRtp({ ...state, ...override }), false);
  }
});

test("fallback memory uses this tab's sessionStorage and ignores legacy localStorage", () => {
  const storage = () => { const values = new Map(); return {
    getItem: key => values.get(key) ?? null, setItem: (key, value) => values.set(key, value),
  }; };
  const legacy = storage(); legacy.setItem(RTC_FAILURE_STORAGE_KEY, "1000");
  const tab = { localStorage: legacy, sessionStorage: storage() };
  assert.equal(rememberedRtcFailure("auto", rtcFailureStorage(tab), 2_000), false);
  rememberRtcFailure(rtcFailureStorage(tab), 2_000);
  assert.equal(rememberedRtcFailure("auto", rtcFailureStorage(tab), 3_000), true);
  assert.equal(rememberedRtcFailure("webrtc", rtcFailureStorage(tab), 3_000), false);
  assert.equal(rememberedRtcFailure("wss", rtcFailureStorage(tab), 3_000), false);
  const newTab = { localStorage: legacy, sessionStorage: storage() };
  assert.equal(rememberedRtcFailure("auto", rtcFailureStorage(newTab), 3_000), false);
  assert.equal(rtcFailureStorage({ get sessionStorage() { throw new Error("denied"); } }), null);
  assert.match(source, /rememberedRtcFailure\(requestedTransport, rtcFailureStorage\(\)\)/);
  assert.doesNotMatch(source, /(?:remember|forget)RtcFailure\(browserStorage\(\)\)/);
});

test("terminal RTC failure falls back in auto mode; explicit RTC and transient disconnection retain policy", () => {
  const fn = source.slice(source.indexOf("function handleRtcPeerConnectionState("), source.indexOf("\n// WSS carries this session:", source.indexOf("function handleRtcPeerConnectionState(")));
  for (const [transport, state, usingWss, expected] of [
    ["auto", "failed", false, "fallback"], ["webrtc", "failed", false, "recycle"],
    ["auto", "disconnected", false, "none"], ["auto", "connecting", false, "none"],
    ["auto", "failed", true, "end-peer"],
  ]) {
    const events = [];
    const context = vm.createContext({ isCurrentPeer: () => true,
      rtcStartupFallback: { progress() {} }, usingWssMedia: usingWss, requestedTransport: transport,
      TRANSPORT: { AUTO: "auto" }, wssSelected: false, rtcPeerEndAction,
      endRtcPeerOnly: () => events.push("end-peer"),
      switchToWss: () => { events.push("fallback"); context.wssSelected = true; },
      ws: { readyState: 1, close: () => events.push("recycle") }, WebSocket: { OPEN: 1 },
      hudState: {}, hud: { classList: { add() {} } }, restartPeer() {}, startFirstFrameHandshake() {}, maybeBindRtcPeer() {},
    });
    vm.runInContext(fn + `\nhandleRtcPeerConnectionState(0, {}, "${state}");`, context);
    assert.deepEqual(events, expected === "none" ? [] : [expected]);
    if (expected === "fallback") {
      vm.runInContext('handleRtcPeerConnectionState(0, {}, "failed");', context);
      assert.deepEqual(events, ["fallback"], "repeat failure cannot recycle the signaling socket during WSS startup");
    }
  }
});

test("ICE failure can rescue auto only once peer connection is no longer connecting", () => {
  const handler = source.slice(source.indexOf("  candidate.oniceconnectionstatechange ="), source.indexOf("  candidate.ontrack ="));
  const events = [];
  const candidate = { iceConnectionState: "failed", connectionState: "connecting" };
  const context = vm.createContext({ candidate, generation: 0, isCurrentPeer: () => true,
    rtcStartupFallback: { progress() {} }, hudState: {}, connected: false,
    requestedTransport: "auto", TRANSPORT: { AUTO: "auto" }, usingWssMedia: false, wssSelected: false,
    switchToWss: reason => events.push(reason),
  });
  vm.runInContext(handler, context);
  candidate.oniceconnectionstatechange(); assert.deepEqual(events, []);
  candidate.connectionState = "failed";
  candidate.oniceconnectionstatechange(); assert.deepEqual(events, ["ice-failed"]);
});
