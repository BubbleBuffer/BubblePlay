import test from "node:test";
import assert from "node:assert/strict";
import { createModifierLatch, LONG_PRESS_MS } from "../public/modifier-latch.js";

const CTRL = 162, SHIFT = 160, ALT = 164;

function harness() {
  const wire = [];
  const timers = new Map();
  let next = 1;
  const latch = createModifierLatch({
    press: vk => wire.push(["down", vk]), release: vk => wire.push(["up", vk]),
    onChange: () => wire.push(["change"]),
    schedule: (cb, ms) => { timers.set(next, { cb, ms }); return next++; },
    cancel: id => timers.delete(id),
  });
  const keys = () => wire.filter(([kind]) => kind !== "change");
  return { latch, wire, timers, keys };
}

test("tap latches, a second tap locks, a third unlocks; a key ends only the one-shot", () => {
  const h = harness();
  h.latch.tap(CTRL);
  assert.equal(h.latch.state(CTRL), "latched");
  h.latch.consumeOneShot();
  assert.equal(h.latch.state(CTRL), "off");
  assert.deepEqual(h.keys(), [["down", CTRL], ["up", CTRL]]);

  h.latch.tap(CTRL); h.latch.tap(CTRL);
  assert.equal(h.latch.state(CTRL), "locked");
  h.latch.consumeOneShot(); h.latch.consumeOneShot();
  assert.equal(h.latch.state(CTRL), "locked");
  h.latch.tap(CTRL);
  assert.equal(h.latch.state(CTRL), "off");
  // Pressed once for the whole latch-and-lock, released once.
  assert.deepEqual(h.keys(), [["down", CTRL], ["up", CTRL], ["down", CTRL], ["up", CTRL]]);
});

test("a long press locks and the release after it is not a tap", () => {
  const h = harness();
  h.latch.pointerDown(SHIFT);
  const [{ cb, ms }] = h.timers.values();
  assert.equal(ms, LONG_PRESS_MS);
  cb();
  h.latch.pointerUp(SHIFT);
  assert.equal(h.latch.state(SHIFT), "locked");
  h.latch.pointerDown(ALT); h.latch.pointerUp(ALT);
  assert.equal(h.latch.state(ALT), "latched");
  assert.equal(h.timers.size, 0);
});

test("hide, blur and disconnect release every held modifier on the PC", () => {
  const h = harness();
  h.latch.tap(CTRL); h.latch.lock(SHIFT);
  h.latch.releaseAll();
  assert.equal(h.latch.active(), false);
  assert.deepEqual(h.keys().filter(([kind]) => kind === "up").map(([, vk]) => vk).sort(), [SHIFT, CTRL].sort());
});

test("a preset chord ignores one-shot modifiers and keeps locks", () => {
  const h = harness();
  h.latch.tap(SHIFT); h.latch.lock(ALT); h.latch.lock(CTRL);
  h.wire.length = 0;
  h.latch.withChord(["ControlLeft", "KeyC"], () => h.wire.push(["chord"]));
  // Shift (one-shot) and Ctrl (the chord presses it) are lifted around the chord, Alt is untouched.
  assert.deepEqual(h.keys().map(([kind, vk]) => kind === "chord" ? "chord" : `${kind}${vk}`),
    [`up${SHIFT}`, `up${CTRL}`, "chord", `down${SHIFT}`, `down${CTRL}`]);
  assert.equal(h.latch.state(SHIFT), "latched");
  assert.equal(h.latch.state(ALT), "locked");
  assert.equal(h.latch.state(CTRL), "locked");
});
