import test from "node:test";
import assert from "node:assert/strict";
import {
  VIEWPORT_REQUEST_MIN_INTERVAL_MS, VIEWPORT_TIMEOUTS_BEFORE_DISABLE, VIEWPORT_TIMEOUT_FULL_REASON,
  VIEWPORT_TRANSITION_TIMEOUT_MS, VIEWPORT_Q16_MAX, createSharpSession,
} from "../public/sharp-session.js";

const DESKTOP = { width: 3840, height: 2160 };

function harness() {
  let clock = 0;
  const timers = new Map();
  let nextTimer = 0;
  const sent = [], recovered = [], logged = [];
  const env = {
    enabled: true, desktop: () => DESKTOP, streamMode: () => "work", canSend: () => true,
    geometry: () => ({ snapshot: { zoom: 2, x: -500, y: -300 }, content: { x: 0, y: 0, width: 1280, height: 720 }, innerWidth: 1280, innerHeight: 720 }),
    send: fields => sent.push(fields), recover: reason => recovered.push(reason), decoded: () => null,
    log: (level, event) => logged.push(event), now: () => clock,
    setTimer: (fn, ms) => { const id = ++nextTimer; timers.set(id, { fn, at: clock + ms }); return id; },
    clearTimer: id => timers.delete(id),
  };
  const session = createSharpSession(env);
  const advance = ms => {
    const end = clock + ms;
    for (;;) {
      const due = [...timers].filter(([, timer]) => timer.at <= end).sort((a, b) => a[1].at - b[1].at)[0];
      if (!due) break;
      clock = due[1].at; timers.delete(due[0]); due[1].fn();
    }
    clock = end;
  };
  return { env, session, sent, recovered, logged, advance };
}

test("a zoomed viewport requests a quantized crop once, and a committed media-view clears the transition", () => {
  const h = harness();
  h.session.schedule("zoom");
  assert.equal(h.sent.length, 0, "waits for the request interval");
  h.advance(VIEWPORT_REQUEST_MIN_INTERVAL_MS);
  assert.equal(h.sent.length, 1);
  const request = h.sent[0];
  assert.equal(request.enabled, true);
  assert.equal(request.reason, "zoom");
  for (const key of ["xQ16", "yQ16", "widthQ16", "heightQ16"]) assert.ok(request[key] >= 0 && request[key] <= VIEWPORT_Q16_MAX, key);
  assert.equal(h.session.isPending, true);
  h.session.onMediaView({ revision: request.revision, state: "sharp", generation: 3, sourceX: 100, sourceY: 50, sourceWidth: 1920, sourceHeight: 1080,
    desktopWidth: DESKTOP.width, desktopHeight: DESKTOP.height });
  assert.equal(h.session.isPending, false);
  assert.deepEqual(h.session.committed, { enabled: true, revision: request.revision });
  assert.equal(h.session.cropForGeneration(3).width, 1920);
  assert.equal(h.session.cropForGeneration(2).generation, 2, "an older generation is drawn with the committed geometry");
});

test("a crop the host never commits asks for a keyframe, then requests the full desktop, and sharp zoom stays off after repeated failures", () => {
  const h = harness();
  const requestSharp = () => { h.session.schedule("zoom"); h.advance(VIEWPORT_REQUEST_MIN_INTERVAL_MS); };
  requestSharp();
  h.advance(VIEWPORT_TRANSITION_TIMEOUT_MS);
  assert.deepEqual(h.recovered, ["viewport-transition-timeout"]);
  assert.equal(h.session.cropForGeneration(1), null, "geometry is unknown while a transition is pending");
  h.advance(VIEWPORT_TRANSITION_TIMEOUT_MS);
  assert.deepEqual(h.logged, ["viewport-transition-failed"]);
  assert.equal(h.sent.at(-1).reason, VIEWPORT_TIMEOUT_FULL_REASON);
  assert.equal(h.sent.at(-1).enabled, false);
  // The full-desktop request is itself unanswered: a second failure disables sharp zoom.
  h.advance(VIEWPORT_TRANSITION_TIMEOUT_MS * 2);
  assert.equal(h.logged.length, VIEWPORT_TIMEOUTS_BEFORE_DISABLE);
  const before = h.sent.length;
  h.session.onMediaView({ revision: h.sent.at(-1).revision, state: "full" });
  requestSharp();
  assert.equal(h.sent.length, before, "sharp zoom is off for the rest of the session");
});
