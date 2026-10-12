import test from "node:test"; import assert from "node:assert/strict";
import { InputController, packet } from "../public/input.js";
import { desktopTransitionPlan } from "../public/monitor-selection.js";

// A controller wired like the receiver: "gated" while a monitor transition
// waits for its first frame, "open" once the new generation presented.
function harness() {
  const env = { state: "open", sent: [] };
  const c = new InputController({ sendControl: data => { env.sent.push([...data]); return true; },
    sendMotion: () => env.state === "open", controlState: () => env.state, scheduleFlush: () => {} });
  return { c, env, releases: () => env.sent.filter(p => p[0] === 6).length };
}

test("a monitor transition keeps a held drag: no release-all when the button stays held", () => {
  const { c, env, releases } = harness();
  assert.equal(c.buttonsAt(100, 100, 1), true);
  env.state = "gated";
  c.absolute(200, 200); assert.equal(c.flushMotion(), false);
  assert.equal(c.cursorSync(), false);          // the old bug: this set needsRelease
  assert.equal(c.buttonsAt(210, 210, 1), false); // repeated press of the held button
  env.state = "open";
  env.sent.length = 0;
  assert.equal(c.resumeAfterTransition(), false);
  assert.equal(releases(), 0);
  assert.equal(c.buttonMask, 1);
  c.buttons(0);
  assert.deepEqual(env.sent, [[...packet.buttons(0)]]);
});

test("a button released during the transition gate sends release-all on resume", () => {
  for (const release of [c => c.buttons(0), c => c.buttonsAt(5, 5, 0)]) {
    const { c, env, releases } = harness();
    c.buttons(1);
    env.state = "gated";
    assert.equal(release(c), false);
    env.state = "open";
    env.sent.length = 0;
    assert.equal(c.resumeAfterTransition(), true);
    assert.equal(releases(), 1);
    assert.equal(c.buttonMask, 0);
    assert.equal(c.needsRelease, false);
    c.key(65, false);
    assert.equal(releases(), 1, "release-all is sent once, not before every later packet");
  }
});

test("keys: a held key let go during the gate releases; gated presses are dropped", () => {
  const { c, env, releases } = harness();
  c.key(16, false);                 // Shift held across the switch
  env.state = "gated";
  c.key(16, false);                 // auto-repeat keeps it held
  c.key(65, false); c.key(65, true); // pressed and released inside the gate
  env.state = "open";
  assert.equal(c.resumeAfterTransition(), false);
  assert.equal(releases(), 0);
  assert.ok(c.held.has(16));
  assert.ok(!c.held.has(65));
  env.state = "gated";
  c.key(16, true);
  env.state = "open";
  assert.equal(c.resumeAfterTransition(), true);
  assert.equal(releases(), 1);
  assert.equal(c.held.size, 0);
});

test("a dead transport still discards with release-all, unlike the transition gate", () => {
  const { c, env, releases } = harness();
  c.buttons(1);
  env.state = "down";
  c.cursorSync();
  env.state = "open";
  c.key(65, false);
  assert.equal(releases(), 1);
});

test("WebRTC transitions stay on the existing peer when frames can be proven; otherwise they recycle it", () => {
  const base = { usingWss: false, alreadyRendering: true, rtcInPlace: true };
  assert.deepEqual(desktopTransitionPlan({ ...base, reason: "monitor-switch", state: "unavailable" }),
    { kind: "monitor", holdInput: true, action: "wait" });
  assert.deepEqual(desktopTransitionPlan({ ...base, reason: "monitor-switched", state: "active" }),
    { kind: "monitor", holdInput: true, action: "handshake" });
  assert.deepEqual(desktopTransitionPlan({ ...base, rtcInPlace: false, reason: "monitor-switched", state: "active" }),
    { kind: "monitor", holdInput: false, action: "restart-peer" });
  assert.equal(desktopTransitionPlan({ ...base, rtcInPlace: false, usingWss: true, reason: "monitor-switched", state: "active" }).action, "handshake");
  for (const reason of ["desktop-switched", "capture-recovered", "session-changed", "agent-restarted"]) {
    assert.equal(desktopTransitionPlan({ ...base, reason, state: "active" }).action, "handshake");
    assert.equal(desktopTransitionPlan({ ...base, reason, state: "active" }).holdInput, false);
    assert.equal(desktopTransitionPlan({ ...base, rtcInPlace: false, reason, state: "active" }).action, "restart-peer");
    assert.equal(desktopTransitionPlan({ ...base, rtcInPlace: false, usingWss: true, reason, state: "active" }).action, "handshake");
  }
  assert.equal(desktopTransitionPlan({ ...base, reason: "monitor-switched", state: "active", alreadyRendering: false }), null);
  assert.equal(desktopTransitionPlan({ ...base, reason: "desktop-switched", state: "unavailable" }), null);
});
