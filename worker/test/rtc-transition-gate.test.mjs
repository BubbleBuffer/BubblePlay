import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";
import {
  desktopTransitionPlan, rtcAdmissionGate, rtcGateOpens, rtcInPlaceCapable, rtpTimestampAtOrAfter,
  transitionKeyframeAdmits, transitionKeyframeVerdict,
} from "../public/monitor-selection.js";

const contract = JSON.parse(readFileSync(new URL("../../fixtures/secure-desktop-contract.json", import.meta.url), "utf8")).webrtcInPlaceTransition;
const SOURCE_REASONS = ["desktop-switched", "capture-recovered", "session-changed", "agent-restarted"];

test("RTP gate compares 32-bit timestamps with serial-number arithmetic", () => {
  for (const { name, admitted, frame, opens } of contract.gateCases) {
    assert.equal(rtpTimestampAtOrAfter(frame, admitted), opens, name);
    assert.equal(rtcGateOpens({ mode: "native", admittedRtpTimestamp: admitted }, { rtpTimestamp: frame }), opens, name);
  }
  assert.equal(rtpTimestampAtOrAfter(-1, 0), false);
  assert.equal(rtpTimestampAtOrAfter(2 ** 32, 0), false);
  assert.equal(rtpTimestampAtOrAfter(1.5, 0), false);
});

test("native presentation without proof never opens an armed gate", () => {
  const gate = { mode: "native", admittedRtpTimestamp: 1000 };
  assert.equal(rtcGateOpens(gate, null), false);
  assert.equal(rtcGateOpens(gate, {}), false);
  assert.equal(rtcGateOpens(gate, { gatePassed: true }), false, "a WebCodecs proof cannot open a native gate");
  assert.equal(rtcGateOpens(null, null), true, "legacy handshakes keep their rule");
  for (const mode of ["worker", "decoder"]) {
    assert.equal(rtcGateOpens({ mode, admittedRtpTimestamp: 1000 }, { rtpTimestamp: 2000 }), false);
    assert.equal(rtcGateOpens({ mode, admittedRtpTimestamp: 1000 }, { gatePassed: true }), true);
  }
});

test("WebCodecs gate needs a keyframe at or after the admitted IDR, or any keyframe without RTP", () => {
  assert.equal(transitionKeyframeAdmits({ isKey: false, frameRtpTimestamp: 5000, admittedRtpTimestamp: 1000 }), false);
  assert.equal(transitionKeyframeAdmits({ isKey: true, frameRtpTimestamp: 999, admittedRtpTimestamp: 1000 }), false);
  assert.equal(transitionKeyframeAdmits({ isKey: true, frameRtpTimestamp: 1000, admittedRtpTimestamp: 1000 }), true);
  assert.equal(transitionKeyframeAdmits({ isKey: true, frameRtpTimestamp: 10, admittedRtpTimestamp: 0xffff_fff0 }), true, "wraparound");
  assert.equal(transitionKeyframeAdmits({ isKey: true, frameRtpTimestamp: undefined, admittedRtpTimestamp: 1000 }), true);
  assert.equal(transitionKeyframeAdmits({ isKey: true, frameRtpTimestamp: 5, admittedRtpTimestamp: null }), true);
});

test("a WebCodecs transition keyframe without an RTP timestamp is unprovable, never admitted", () => {
  const strict = { strict: true, admittedRtpTimestamp: 1000 };
  assert.equal(transitionKeyframeVerdict({ ...strict, isKey: true, frameRtpTimestamp: undefined }), "unprovable");
  assert.equal(transitionKeyframeVerdict({ ...strict, isKey: true, frameRtpTimestamp: 1000 }), "admit");
  assert.equal(transitionKeyframeVerdict({ ...strict, isKey: true, frameRtpTimestamp: 999 }), "drop");
  assert.equal(transitionKeyframeVerdict({ ...strict, isKey: false, frameRtpTimestamp: undefined }), "drop");
  assert.equal(transitionKeyframeVerdict({ strict: true, isKey: true, frameRtpTimestamp: 5, admittedRtpTimestamp: null }), "unprovable");
  assert.equal(transitionKeyframeAdmits({ ...strict, isKey: true, frameRtpTimestamp: undefined }), false);
  // Startup and post-restart handshakes keep keyframe-after-admission.
  assert.equal(transitionKeyframeVerdict({ isKey: true, frameRtpTimestamp: undefined, admittedRtpTimestamp: 1000 }), "admit");
});

test("admission plans a gate, or a peer restart when the transition cannot be proven", () => {
  const native = { path: "native", rvfc: true, rtpTimestamps: true };
  assert.deepEqual(rtcAdmissionGate({ ...native, transition: true, admittedRtpTimestamp: 77 }),
    { action: "gate", gate: { mode: "native", admittedRtpTimestamp: 77 } });
  assert.equal(rtcAdmissionGate({ ...native, transition: true, admittedRtpTimestamp: undefined }).action, "restart-peer", "older host");
  assert.equal(rtcAdmissionGate({ ...native, rvfc: false, transition: true, admittedRtpTimestamp: 77 }).action, "restart-peer", "no rVFC");
  assert.equal(rtcAdmissionGate({ ...native, rtpTimestamps: false, transition: true, admittedRtpTimestamp: 77 }).action, "restart-peer", "no rtpTimestamp");
  assert.equal(rtcAdmissionGate({ ...native, rvfc: false, transition: false, admittedRtpTimestamp: 77 }).action, "legacy");
  assert.equal(rtcAdmissionGate({ path: "worker", transition: true, admittedRtpTimestamp: undefined }).action, "restart-peer");
  assert.deepEqual(rtcAdmissionGate({ path: "worker", transition: true, admittedRtpTimestamp: 5 }),
    { action: "gate", gate: { mode: "worker", admittedRtpTimestamp: 5, strict: true } });
  assert.deepEqual(rtcAdmissionGate({ path: "decoder", transition: false, admittedRtpTimestamp: undefined }),
    { action: "gate", gate: { mode: "decoder", admittedRtpTimestamp: null, strict: false } });
});

test("desktop-source and monitor transitions on WebRTC stay in place only with the capability", () => {
  const base = { usingWss: false, alreadyRendering: true, state: "active" };
  const capable = rtcInPlaceCapable({ path: "native", rvfc: true, rtpTimestamps: true });
  assert.equal(capable, true);
  assert.equal(rtcInPlaceCapable({ path: "native", rvfc: true, rtpTimestamps: false }), false);
  assert.equal(rtcInPlaceCapable({ path: "native", rvfc: false, rtpTimestamps: true }), false);
  assert.equal(rtcInPlaceCapable({ path: "worker" }), true);
  for (const reason of SOURCE_REASONS) {
    assert.deepEqual(desktopTransitionPlan({ ...base, reason, rtcInPlace: capable }), { kind: "source", holdInput: false, action: "handshake" });
    assert.equal(desktopTransitionPlan({ ...base, reason, rtcInPlace: false }).action, "restart-peer");
  }
  assert.deepEqual(desktopTransitionPlan({ ...base, reason: "monitor-switched", rtcInPlace: true }), { kind: "monitor", holdInput: true, action: "handshake" });
  assert.deepEqual(desktopTransitionPlan({ ...base, reason: "monitor-switched", rtcInPlace: false }), { kind: "monitor", holdInput: false, action: "restart-peer" });
});

// Runs the receiver's real handshake functions in a sandbox.
function receiverHarness({ path = "native", reason = "desktop-source-transition", rtpTimestamps = true, lastRtpTimestamp = null } = {}) {
  const source = readFileSync(new URL("../public/receiver.js", import.meta.url), "utf8");
  const start = source.indexOf("function clearFirstFrameWatchdog()");
  const end = source.indexOf("let monitorList = [];");
  assert.ok(start > 0 && end > start);
  const restarts = [], signals = [], resyncs = [];
  const handshake = { requestId: "firstFrame_test1", reason, transport: "webrtc", epoch: null, generation: 0, phase: "starting",
    retries: 0, admitted: false, presented: false, requireAdmission: true, rtpGate: null, failureReason: "", timer: null };
  const context = vm.createContext({
    rtcAdmissionGate, rtcGateOpens, rtcInPlaceCapable, rtpTimestampAtOrAfter,
    firstFrameHandshake: handshake, usingWssMedia: false, useWebCodecs: path !== "native",
    activeVideo: path === "native" ? {} : null, encodedWorker: path === "worker" ? {} : null, peerGeneration: 3,
    nativePresentation: path === "native" ? { generation: 3, supported: true, rtpTimestamps, lastRtpTimestamp } : null,
    transitionNeedsKeyframe: false, transitionGatePassedToken: null, labQualityKeyframeAcks: new Map(),
    wssStartupRecoveries: 0, hasPresentedFrame: false, desktopAwaitingFrame: true, suppressDesktopTransitionOverlay: false,
    inputTransitionHold: false, inputReady: true, fileDropPresentedGeneration: null, fileDropPresentedViewportRevision: null,
    input: { resumeAfterTransition() {} }, onInputReady() {}, showContextHint() {}, renderDesktopState() {}, debugTrace() {},
    clearFirstFrameWatchdog() {}, recoverWssStartup() {}, sendClientLog() {}, forgetRtcFailure() {}, rtcFailureStorage() {},
    rtcStartupFallback: { progress() {}, stop() {}, requestSent() {} }, document: { body: { classList: { add() {} } } },
    resyncPipeline: (why, gate) => resyncs.push({ why, gate }), sendSignal: (message) => signals.push(message),
    recycleSignaling() {}, restartPeer() {},
    restartPeerInNewEpoch: (_fns, why) => restarts.push(why),
    ws: { readyState: 1 }, WebSocket: { OPEN: 1 }, rtcOfferSignaled: true, mediaEpoch: null, desktopAvailable: true,
    crypto: globalThis.crypto, btoa: globalThis.btoa, setTimeout: () => 1, clearTimeout() {},
    codecSession: { codec: "h264" }, WSS_PROTOCOL: 1,
  });
  vm.runInContext(source.slice(start, end), context);
  const run = (code) => vm.runInContext(code, context);
  return { handshake, restarts, signals, resyncs, context, run,
    admit: (rtpTimestamp) => run(`handleFirstFrameMessage(${JSON.stringify({ type: "first-frame", protocol: 1, phase: "admitted", requestId: "firstFrame_test1", transport: "webrtc", generation: 12, ...(rtpTimestamp === undefined ? {} : { rtpTimestamp }) })})`),
    present: (proof) => run(`markFirstFramePresented(null, null, ${JSON.stringify(proof)})`) };
}

test("receiver: a stale native frame does not open the gate; a frame at the admitted IDR does", () => {
  const h = receiverHarness();
  h.admit(4_294_960_000);
  assert.deepEqual(h.restarts, []);
  assert.equal(h.present({ rtpTimestamp: 4_294_959_999 }), false);
  assert.equal(h.present(null), false, "loadeddata/playing carry no proof");
  assert.equal(h.context.desktopAwaitingFrame, true);
  assert.equal(h.present({ rtpTimestamp: 1_000 }), true, "after wraparound");
  assert.equal(h.context.desktopAwaitingFrame, false);
  assert.equal(h.signals.at(-1).phase, "presented");
});

test("receiver: a native frame presented before the admission message counts only if it reached the IDR", () => {
  const early = receiverHarness({ lastRtpTimestamp: 90_000 });
  early.admit(90_000);
  assert.equal(early.handshake.presented, true);
  const stale = receiverHarness({ lastRtpTimestamp: 89_999 });
  stale.admit(90_000);
  assert.equal(stale.handshake.presented, false);
});

test("receiver: missing host timestamp or rVFC rtpTimestamp falls back to a peer restart with input gated", () => {
  for (const reason of ["desktop-source-transition", "monitor-transition"]) {
    const old = receiverHarness({ reason });
    old.admit(undefined);
    assert.deepEqual(old.restarts, [reason]);
    assert.equal(old.handshake.admitted, false);
    assert.equal(old.context.desktopAwaitingFrame, true);
    const noRtp = receiverHarness({ reason, rtpTimestamps: false });
    noRtp.admit(5);
    assert.deepEqual(noRtp.restarts, [reason]);
  }
  // A startup handshake without the timestamp keeps the legacy rule.
  const startup = receiverHarness({ reason: "desktop-state" });
  startup.admit(undefined);
  assert.deepEqual(startup.restarts, []);
  assert.equal(startup.present(null), true);
});

test("receiver: monitor transitions use the same RTP gate", () => {
  const h = receiverHarness({ reason: "monitor-transition" });
  h.admit(500);
  assert.equal(h.present({ rtpTimestamp: 499 }), false);
  assert.equal(h.present({ rtpTimestamp: 500 }), true);
});

test("receiver: the worker path resyncs with the gate and opens only on its token", () => {
  const h = receiverHarness({ path: "worker" });
  h.admit(700);
  assert.equal(h.resyncs.length, 1);
  assert.equal(h.resyncs[0].gate.token, "firstFrame_test1");
  assert.equal(h.resyncs[0].gate.admittedRtpTimestamp, 700);
  assert.equal(h.resyncs[0].gate.strict, true, "a frame without rtpTimestamp cannot pass");
  assert.equal(h.present({ gatePassed: false }), false);
  assert.equal(h.present({ rtpTimestamp: 900 }), false);
  assert.equal(h.present({ gatePassed: true }), true);
});

test("worker drops frames until the admitted keyframe and counts draws for that token", () => {
  const source = readFileSync(new URL("../public/latency-worker.js", import.meta.url), "utf8");
  assert.match(source, /import \{ transitionKeyframeVerdict \} from "\/monitor-selection\.js";/);
  assert.match(source, /if \(transitionGate\) \{[\s\S]*?if \(transitionGate\.unprovable\) \{ stats\.dropped\+\+; return; \}[\s\S]*?strict: transitionGate\.strict \}\);\s*if \(verdict === "unprovable"\) \{ transitionGate\.unprovable = true; stats\.dropped\+\+; control\("transition-unprovable", \{ token: stats\.transitionToken \}\); return; \}\s*if \(verdict !== "admit"\) \{[\s\S]*?return; \}\s*transitionGate = null; stats\.transitionPassed = true;/);
  assert.match(source, /strict: data\.transition\.strict === true, unprovable: false/);
  const receiver = readFileSync(new URL("../public/receiver.js", import.meta.url), "utf8");
  assert.match(receiver, /message\.type === "transition-unprovable"[\s\S]{0,200}message\.token === firstFrameHandshake\.requestId\) recoverRtcInPlaceTransition\("no-frame-rtp-timestamp"\)/);
  assert.match(receiver, /strict: transition\.strict === true/);
  assert.match(source, /stats\.framesDrawn\+\+; if \(stats\.transitionPassed\) stats\.transitionDrawn\+\+;/);
  assert.match(source, /stats\.transitionToken = data\.transition\.token; stats\.transitionDrawn = 0; stats\.transitionPassed = false;/);
});

test("receiver: a transition's fallback restart forces an admitted, RTP-gated handshake on the new peer", () => {
  const h = receiverHarness({ reason: "monitor-transition" });
  h.run(`recoverRtcInPlaceTransition("presentation-timeout")`);
  assert.deepEqual(h.restarts, ["monitor-transition"]);
  // restartPeer clears the old request; the new peer connects.
  Object.assign(h.handshake, { requestId: null, phase: "idle", admitted: false, presented: false });
  h.run(`startFirstFrameHandshake("peer-connected")`);
  assert.equal(h.handshake.requireAdmission, true);
  assert.equal(h.handshake.reason, "peer-connected");
  h.handshake.requestId = "firstFrame_test1";
  assert.equal(h.present({ rtpTimestamp: 900 }), false, "nothing presents before admission");
  h.admit(800);
  assert.deepEqual(h.restarts, ["monitor-transition"], "no second restart");
  assert.equal(h.handshake.rtpGate.mode, "native");
  assert.equal(h.present({ rtpTimestamp: 799 }), false);
  assert.equal(h.present({ rtpTimestamp: 800 }), true);
  // The flag is consumed: a later ordinary startup keeps the legacy rule.
  Object.assign(h.handshake, { requestId: null, phase: "idle" });
  h.run(`startFirstFrameHandshake("peer-connected")`);
  assert.equal(h.handshake.requireAdmission, false);
});

test("receiver: a restarted peer without a host timestamp falls back to admission, not another restart", () => {
  const h = receiverHarness({ reason: "desktop-source-transition", rtpTimestamps: false });
  h.run(`restartRtcForTransition("desktop-source-transition")`);
  Object.assign(h.handshake, { requestId: null, phase: "idle", admitted: false, presented: false });
  h.run(`startFirstFrameHandshake("ice-connected")`);
  h.handshake.requestId = "firstFrame_test1";
  h.admit(undefined);
  assert.deepEqual(h.restarts, ["desktop-source-transition"]);
  assert.equal(h.handshake.admitted, true);
  assert.equal(h.present(null), true);
});

test("receiver: a desktop-state after presentation starts a new handshake instead of stalling input", () => {
  const h = receiverHarness({ reason: "desktop-state" });
  for (const phase of ["presented", "ready", "idle", "failed"]) {
    h.handshake.phase = phase;
    assert.equal(h.run(`desktopStateNeedsNewHandshake(false)`), true, phase);
  }
  for (const phase of ["starting", "retrying", "in-flight"]) {
    h.handshake.phase = phase;
    assert.equal(h.run(`desktopStateNeedsNewHandshake(false)`), false, phase);
    assert.equal(h.run(`desktopStateNeedsNewHandshake(true)`), true, phase);
  }
  Object.assign(h.handshake, { phase: "presented", presented: true });
  const before = h.handshake.requestId;
  h.run(`startFirstFrameHandshake("desktop-state", desktopStateNeedsNewHandshake(false))`);
  assert.notEqual(h.handshake.requestId, before);
  assert.equal(h.handshake.phase, "starting");
  assert.equal(h.handshake.presented, false);
  assert.ok(h.signals.some((m) => m.type === "first-frame" && m.phase === "request" && m.requestId === h.handshake.requestId));
});
