import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";
import { createRecoveryTracker } from "../public/recovery-tracker.js";
import { OFFER_FAILURE_LIMIT } from "../public/recovery.js";
import { createSignalingSocket } from "../public/signaling-socket.js";
import { STUN_FALLBACK } from "../public/turn.js";
import { createCodecSession, TRANSPORT } from "../public/transport.js";
import { FakePeerConnection, peerHarness } from "./helpers/peer-harness.mjs";

const hevc = { mimeType: "video/H265", clockRate: 90000, sdpFmtpLine: "profile-id=1" };
const h264 = { mimeType: "video/H264", clockRate: 90000, sdpFmtpLine: "packetization-mode=1;profile-level-id=640c1f" };
const STARTUP_CHECK_MS = 5_000;
const flush = async () => { for (let i = 0; i < 6; i++) await Promise.resolve(); };

test("start builds a receive-only video and audio peer with the two input lanes and offers it", async () => {
  const h = peerHarness();
  await h.peer.start();
  const [pc] = h.pcs;
  assert.deepEqual(pc.options, { iceServers: h.state.currentIceServers, iceTransportPolicy: "all" });
  assert.deepEqual(pc.transceivers.map(t => [t.kind, t.init.direction]), [["video", "recvonly"], ["audio", "recvonly"]]);
  assert.equal(pc.transceivers[0].receiver.jitterBufferTarget, 0);
  assert.deepEqual(pc.channels.map(c => [c.label, c.options]),
    [["input-control", { ordered: true }], ["input-motion", { ordered: false, maxRetransmits: 0 }]]);
  assert.equal(h.state.pc, pc);
  assert.equal(h.state.dc, pc.channels[0]);
  assert.deepEqual(h.calls, [["send-control", "offer"], ["first-frame", "offer-sent"]]);
  assert.equal(h.state.rtcOfferSignaled, true);

  const refused = peerHarness({ controlSent: false });
  await refused.peer.start();
  assert.deepEqual(refused.calls, [["send-control", "offer"]]);
  assert.equal(refused.state.rtcOfferSignaled, false);

  const webCodecs = peerHarness({ decoderPath: "webcodecs" });
  await webCodecs.peer.start();
  assert.equal(webCodecs.pcs[0].options.encodedInsertableStreams, true);
});

test("start does nothing without an authenticated control channel, an open socket, a current generation or a WebRTC transport", async () => {
  const cases = [
    { transport: TRANSPORT.WSS },
    { state: { authenticatedSession: { controlReady: false } } },
    { state: { ws: { readyState: 3 } } },
    { state: { offerSent: true } },
    { state: { usingWssMedia: true } },
  ];
  for (const options of cases) {
    const h = peerHarness(options);
    await h.peer.start();
    assert.equal(h.pcs.length, 0, JSON.stringify(options));
  }
  const stale = peerHarness();
  await stale.peer.start(stale.state.peerGeneration + 1);
  assert.equal(stale.pcs.length, 0);
});

test("codec preferences put HEVC first, and fall back to H.264 with a new peer when HEVC is not offered", async () => {
  for (const [codec, caps, expected, rebuilt] of [
    ["hevc", [h264, hevc], [hevc, h264], false],
    ["hevc", [h264], [h264], true],
    ["hevc", [], null, true],
    ["h264", [hevc, h264], [h264], false],
    ["h264", [hevc], null, false],
  ]) {
    const codecSession = createCodecSession({ transport: "webrtc", decoderPath: codec === "h264" ? "webcodecs" : "native" });
    const h = peerHarness({ env: { codecSession, receiverCapabilities: () => ({ codecs: caps }) } });
    await h.peer.start();
    await flush();
    assert.equal(h.pcs.length, rebuilt ? 2 : 1, `${codec} ${caps.length}`);
    assert.equal(h.pcs[0].closed, rebuilt);
    assert.deepEqual(h.pcs.at(-1).transceivers[0].preferences ?? null, expected);
    assert.equal(codecSession.codec, rebuilt ? "h264" : codec);
  }
});

test("repeated offer failures stop restarting the peer and hand over to the WSS fallback", async () => {
  for (const [transport, wss] of [[TRANSPORT.AUTO, ["offer-failed"]], [TRANSPORT.WEBRTC, []]]) {
    const h = peerHarness({ transport, env: { createPeerConnection: options => {
      const pc = new FakePeerConnection(options); pc.offerError = new Error("sdp"); h.pcs.push(pc); return pc; } } });
    await h.peer.start();
    for (let i = 0; i < OFFER_FAILURE_LIMIT * 2; i++) await flush();
    const restarts = h.calls.filter(call => call[0] === "invalidate-monitor");
    assert.equal(restarts.length, OFFER_FAILURE_LIMIT - 1, "each failure below the limit restarts the peer");
    assert.deepEqual(restarts[0], ["invalidate-monitor", "offer:sdp"]);
    assert.deepEqual(h.calls.filter(call => call[0] === "switch-to-wss").map(call => call[1]), wss);
    if (!wss.length) assert.equal(h.env.hudState.ice, "offer-failed");
  }
});

test("a throwing RTCPeerConnection retries on the STUN fallback, then reaches WSS in auto mode and the error state when forced", async () => {
  for (const [transport, wss, hudIce] of [[TRANSPORT.AUTO, ["rtc-construct-failed"], undefined], [TRANSPORT.WEBRTC, [], "ice-config-error"]]) {
    const constructed = [];
    const h = peerHarness({ transport, state: { currentIceServers: [{ urls: "turn:x.test" }] }, env: {
      createPeerConnection: options => { constructed.push(options); throw new Error("resource limit"); } } });
    await h.peer.start();
    assert.equal(constructed.length, 2, "one retry on the STUN fallback");
    assert.equal(constructed[1].iceServers, STUN_FALLBACK);
    assert.deepEqual(h.calls.filter(call => call[0] === "switch-to-wss").map(call => call[1]), wss);
    assert.equal(h.env.hudState.ice, hudIce);
    assert.equal(h.state.offerSent, false);
    assert.equal(h.state.pc, null);
  }
});

test("ICE failure rescues auto mode only once the peer connection is no longer connecting; a connected ICE state renews the first-frame handshake", async () => {
  const h = peerHarness();
  await h.peer.start();
  h.calls.length = 0;
  const pc = h.pcs[0];
  pc.iceConnectionState = "failed"; pc.connectionState = "connecting";
  pc.oniceconnectionstatechange();
  assert.deepEqual(h.calls, []);
  pc.connectionState = "failed";
  pc.oniceconnectionstatechange();
  assert.deepEqual(h.calls, [["switch-to-wss", "ice-failed"]]);
  h.calls.length = 0;
  pc.iceConnectionState = "connected";
  pc.oniceconnectionstatechange();
  assert.deepEqual(h.calls, [["mark-good"], ["pointer-lock"], ["first-frame", "ice-connected"]]);
  assert.equal(h.state.connected, true);
  pc.onconnectionstatechange();
  assert.deepEqual(h.calls.at(-1), ["connection-state", h.state.peerGeneration, pc, "failed"]);
});

test("auto mode arms the RTP startup fallback with the receiver's frame counts; a forced WebRTC page has none", async t => {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  const healthy = peerHarness({ env: { startupSnapshot: () => ({ framesReceived: 0, nativeFramesPresented: 1 }) } });
  await healthy.peer.start();
  healthy.state.rtcStartupFallback.requestSent();
  t.mock.timers.tick(STARTUP_CHECK_MS);
  await flush();
  assert.deepEqual(healthy.calls.filter(call => call[0] === "rtc-healthy" || call[0] === "switch-to-wss"), [["rtc-healthy"]]);
  const forced = peerHarness({ transport: TRANSPORT.WEBRTC });
  await forced.peer.start();
  assert.equal(forced.state.rtcStartupFallback, null);
});

test("tracks are disabled and held until the media gate opens, at most one per transceiver, and refused when WSS carries the session", async () => {
  const h = peerHarness();
  await h.peer.start();
  const pc = h.pcs[0];
  const track = () => ({ enabled: true, stopped: false, stop() { this.stopped = true; } });
  const [video, audio, extra] = [track(), track(), track()];
  for (const t of [video, audio, extra]) pc.ontrack({ track: t });
  assert.deepEqual([video.enabled, audio.enabled], [false, false]);
  assert.equal(h.state.pendingRtcTracks.length, 2);
  assert.equal(extra.stopped, true);
  assert.equal(h.names().includes("track"), false);
  h.state.authenticatedSession.mediaReady = true;
  const live = track();
  pc.ontrack({ track: live });
  assert.deepEqual(h.calls.at(-1)[0], "track");
  h.state.usingWssMedia = true;
  const refused = track();
  pc.ontrack({ track: refused });
  assert.equal(refused.stopped, true);
});

test("cursor state on either lane is handled only while media is ready and WebRTC carries the session", async () => {
  const h = peerHarness();
  await h.peer.start();
  const [control, motion] = h.pcs[0].channels;
  control.onmessage({ data: "early" });
  h.state.authenticatedSession.mediaReady = true;
  control.onmessage({ data: "control" });
  motion.onmessage({ data: "motion" });
  h.state.usingWssMedia = true;
  motion.onmessage({ data: "wss" });
  assert.deepEqual(h.calls.filter(call => call[0] === "cursor"), [["cursor", "control"], ["cursor", "motion"]]);
});

test("a lane closing under a live peer recycles signaling and restarts without reconnecting; under WSS it ends only the RTC peer", async () => {
  const h = peerHarness();
  await h.peer.start();
  h.calls.length = 0;
  h.pcs[0].channels[0].onclose();
  assert.deepEqual(h.names().slice(0, 3), ["release-input", "recycle-signaling", "invalidate-monitor"]);
  assert.equal(h.calls.some(call => call[0] === "reconnect-signaling" || call[0] === "end-rtc-peer-only"), false);

  const wss = peerHarness({ transport: TRANSPORT.AUTO, state: { wssSelected: true } });
  wss.state.usingWssMedia = false;
  wss.state.offerSent = false;
  await wss.peer.start();
  wss.calls.length = 0;
  wss.pcs[0].channels[1].onclose();
  assert.deepEqual(wss.calls, [["end-rtc-peer-only", "input-lane-closed"]]);
});

test("cleanup stops held tracks, releases input before removing the lanes, and closes the peer", async () => {
  const h = peerHarness();
  await h.peer.start();
  const pc = h.pcs[0];
  const held = { stop: () => h.calls.push(["track-stop"]) };
  h.state.pendingRtcTracks = [{ event: { track: held }, generation: 1 }];
  h.state.inputReady = true; h.state.connected = true; h.state.fileDropPresentedGeneration = 3;
  h.calls.length = 0;
  h.peer.cleanup();
  assert.deepEqual(h.names(), ["track-stop", "reset-file-drop", "release-input", "detach-sound", "clear-wss-timers", "peer-reset", "reset-input-ui", "release-media"]);
  assert.deepEqual([pc.closed, pc.channels.every(c => c.closed)], [true, true]);
  assert.deepEqual([h.state.pc, h.state.dc, h.state.motionDc, h.state.pendingRtcTracks.length, h.state.inputReady, h.state.connected,
    h.state.closingPeer, h.state.fileDropPresentedGeneration], [null, null, null, 0, false, false, false, null]);
  h.state.usingWssMedia = true;
  h.calls.length = 0;
  h.peer.cleanup();
  assert.ok(h.names().indexOf("stop-wss") > h.names().indexOf("release-input"), "a WSS session stops its media after releasing input");
});

test("restart drops the old epoch, then starts the new peer, WSS media or a reconnect", async () => {
  const h = peerHarness();
  const before = h.state.peerGeneration;
  h.state.pendingRemoteIce = ["ice"]; h.state.remoteDescriptionSet = true; h.state.offerSent = true;
  h.peer.restart("user-retry", true);
  await flush();
  assert.equal(h.state.peerGeneration, before + 1);
  assert.deepEqual(h.names().slice(0, 4), ["invalidate-monitor", "reset-handshake", "render-desktop", "clear-host-quality"]);
  assert.deepEqual([h.state.pendingRemoteIce.length, h.state.remoteDescriptionSet, h.state.desktopAwaitingFrame, h.env.hudState.ice], [0, false, true, "user-retry"]);
  assert.equal(h.pcs.length, 1, "an open socket gets a new peer in the new generation");

  const wss = peerHarness({ state: { wssSelected: true } });
  wss.peer.restart("wss-restart");
  assert.equal(wss.calls.some(call => call[0] === "start-wss"), true);
  assert.equal(wss.pcs.length, 0);

  const closed = peerHarness({ state: { ws: { readyState: 3 } } });
  closed.peer.restart("socket-gone", true);
  closed.peer.restart("socket-gone-quietly", false);
  assert.deepEqual(closed.calls.filter(call => call[0] === "reconnect-signaling"), [["reconnect-signaling", "socket-gone"]]);
});

test("peer-left recycles signaling first, then restarts the peer without a second reconnect", async () => {
  const source = readFileSync(new URL("../public/receiver.js", import.meta.url), "utf8").replaceAll("\r\n", "\n");
  const handleSignalSource = source.slice(source.indexOf("async function handleSignal("), source.indexOf("\nasync function handleEndpointSignal("));
  const sockets = [], timers = [];
  const h = peerHarness({ state: { signalingRegistered: true, ws: null } });
  class FakeSocket {
    constructor() { this.readyState = 1; sockets.push(this); }
    close(code, reason) { this.readyState = 3; h.calls.push(["socket-close", code, reason]); }
  }
  const signaling = createSignalingSocket({
    state: h.state, recovery: createRecoveryTracker({ now: () => 0, log: () => {} }), hudState: h.env.hudState,
    url: () => "wss://relay.test/ws?pc=pc", hello: () => ({ type: "hello" }), handleSignal: async () => {},
    cleanupPeer: h.peer.cleanup, restartPeer: h.peer.restart, markBad: () => {}, onPolicyClose: () => {}, onMessageFailed: () => {},
    invalidateMonitor: reason => h.calls.push(["signaling-invalidate-monitor", reason]), recycleDiagnostic: reason => ({ reason }),
    log: () => {}, trace: () => {}, recordError: () => {}, now: () => 0, makeSocket: () => new FakeSocket(), random: () => 0.5,
    setTimeout: (fn, delay) => { timers.push({ fn, delay }); return timers.length; }, clearTimeout: () => {},
  });
  signaling.reconnect("startup");
  const socket = sockets[0];
  const generation = h.state.signalingGeneration;
  const before = h.state.peerGeneration;
  const context = vm.createContext({ sessionState: h.state, console, JSON, recycleSignaling: signaling.recycle, restartPeer: h.peer.restart });
  vm.runInContext(handleSignalSource, context);
  h.calls.length = 0;
  await vm.runInContext("handleSignal", context)(socket, generation, { data: JSON.stringify({ type: "peer-left" }) });
  assert.deepEqual(h.names().slice(0, 3), ["signaling-invalidate-monitor", "socket-close", "invalidate-monitor"], "the recycle closes the socket before the peer restarts");
  assert.deepEqual(h.calls.find(call => call[0] === "socket-close"), ["socket-close", 1000, "peer-left"]);
  assert.equal(h.state.signalingGeneration, generation + 1);
  assert.equal(h.state.peerGeneration, before + 1, "then the peer restarts");
  assert.deepEqual(h.calls.filter(call => call[0] === "invalidate-monitor"), [["invalidate-monitor", "peer-left"]]);
  assert.equal(h.names().includes("reconnect-signaling"), false, "the recycle owns the reconnect");
  assert.equal(h.pcs.length, 0);
  assert.equal(timers.length, 1, "one reconnect is scheduled");
});
