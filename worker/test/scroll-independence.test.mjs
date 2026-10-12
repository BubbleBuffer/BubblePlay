import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { InputController, touchTranslateWheel, TouchGesture, WheelAccumulator } from "../public/input.js";

test("large native wheel samples retain their complete distance through the accumulator and sender", () => {
  const { c, wire } = harness();
  const units = new WheelAccumulator().add(50000, 100000);
  c.motion(0, 0, units.x, units.y); c.flushMotion();
  assert.deepEqual(summarize(wire), { absolutes: [], wheel: { x: 60000, y: -120000 } });
});

test("a very large retained wheel backlog yields between frames and eventually sends every unit", () => {
  const { c, wire } = harness();
  c.motion(0, 0, 2000000, -2000000);
  assert.equal(c.flushMotion(), true);
  assert.equal(c.hasPendingMotion(), true, "one frame leaves work for later frames");
  drain(c);
  assert.deepEqual(summarize(wire), { absolutes: [], wheel: { x: 2000000, y: -2000000 } });
});

test("fractional touch wheel distance survives frames and pointer barriers until it forms a whole unit", () => {
  const { c, wire } = harness();
  for (let i = 0; i < 3; i++) {
    c.motionNow(0, 0, -0.25, 0.25);
    assert.equal(c.hasPendingMotion(), false, "fraction alone does not arm a retry loop");
  }
  c.absolute(10, 20); c.flushMotion();
  c.buttonsAt(10, 20, 0);
  c.motionNow(0, 0, -0.25, 0.25);
  assert.deepEqual(summarize(wire), { absolutes: [[10, 20]], wheel: { x: -1, y: 1 } });
});

// Flushes until the controller holds nothing, however many packets that takes.
// Assertions read what was sent, not how many sequence numbers it used.
function drain(controller, limit = 16) {
  for (let i = 0; i < limit && controller.hasPendingMotion(); i++) controller.flushMotion();
  assert.equal(controller.hasPendingMotion(), false, "motion drains within the limit");
  return controller;
}

// Records every packet on one wire in send order, tagged by lane. `accept` decides
// whether a motion packet is taken (false = refused by the transport).
function harness({ accept = () => true, control = () => "open", buffered = () => 0 } = {}) {
  const wire = [];
  const c = new InputController({
    scheduleFlush: () => {},
    controlState: control,
    motionBuffered: buffered,
    sendMotion: p => { if (!accept([...p])) return false; wire.push({ lane: "motion", bytes: [...p] }); return true; },
    sendControl: p => { wire.push({ lane: "control", bytes: [...p] }); return true; },
  });
  return { c, wire };
}

const view = bytes => new DataView(Uint8Array.from(bytes).buffer);
const seqOf = bytes => bytes[1] | (bytes[2] << 8);
const types = wire => wire.map(w => w.bytes[0]);
const seqs = wire => wire.map(w => seqOf(w.bytes));

// Sums the wheel fields of every motion packet on the wire and reads absolute targets.
function summarize(wire) {
  const absolutes = [], wheel = { x: 0, y: 0 };
  for (const { bytes } of wire) {
    const v = view(bytes);
    if (bytes[0] === 8) absolutes.push([v.getUint16(3, true), v.getUint16(5, true)]);
    if (bytes[0] === 7) { wheel.x += v.getInt16(7, true); wheel.y += v.getInt16(9, true); }
  }
  return { absolutes, wheel };
}

function assertConsecutiveSequences(wire, label) {
  const list = seqs(wire);
  assert.equal(new Set(list).size, list.length, `${label}: no sequence number is reused`);
  for (let i = 1; i < list.length; i++) assert.equal(list[i], (list[i - 1] + 1) & 0xffff, `${label}: shared sequence is consecutive`);
}

test("a wheel and an absolute target in the same frame both reach the host, in either order", () => {
  for (const order of ["absolute-first", "wheel-first"]) {
    const { c, wire } = harness();
    if (order === "absolute-first") { c.absolute(10, 20); c.motion(0, 0, 0, 60); }
    else { c.motion(0, 0, 0, 60); c.absolute(10, 20); }
    drain(c);
    assert.deepEqual(summarize(wire), { absolutes: [[10, 20]], wheel: { x: 0, y: 60 } }, order);
  }
});

test("absolute target then wheel go out in one flush on consecutive shared sequences", () => {
  const { c, wire } = harness();
  c.absolute(10, 20); c.motion(0, 0, 0, 60);
  assert.equal(c.flushMotion(), true);
  assert.deepEqual(types(wire), [8, 7], "one flush sends the target, then the wheel");
  assert.deepEqual(seqs(wire), [0, 1]);
  assert.equal(c.hasPendingMotion(), false);
  assert.equal(c.lastCursorSequence, 0, "cursor authority stays with the target");
  assert.equal(c.lastSentSequence, 1, "the last packet is the wheel");
});

test("a refused wheel tail stays pending and the retry delivers it, with nothing lost", () => {
  let calls = 0;
  const { c, wire } = harness({ accept: () => ++calls !== 3 });  // absolute ok, first wheel chunk ok, second refused
  c.absolute(10, 20); c.motion(0, 0, 40000, -70000);
  assert.equal(c.flushMotion(), true, "the target and the first chunk went out");
  assert.equal(c.hasPendingMotion(), true, "the refused remainder is still pending");
  assert.equal(c.stats.dropped, 1);
  drain(c);
  assert.deepEqual(summarize(wire), { absolutes: [[10, 20]], wheel: { x: 40000, y: -70000 } });
  assertConsecutiveSequences(wire, "partial send");
  assert.equal(c.seq, 4, "the refused packet's sequence was given back and reused");
});

test("large wheel distance is split into i16 packets in one flush and never clamped away", () => {
  const { c, wire } = harness();
  c.motion(0, 0, 40000, -100000);
  assert.equal(c.flushMotion(), true);
  assert.equal(c.hasPendingMotion(), false, "everything went out in one flush");
  assert.equal(wire.length, 4);
  for (const { bytes } of wire) {
    assert.equal(bytes[0], 7);
    for (const offset of [7, 9]) assert.ok(view(bytes).getInt16(offset, true) >= -32768 && view(bytes).getInt16(offset, true) <= 32767);
  }
  assert.deepEqual(summarize(wire), { absolutes: [], wheel: { x: 40000, y: -100000 } });
  assertConsecutiveSequences(wire, "large distance");
});

test("buttonsAt and cursorSync keep pending wheel distance and send it after the barrier", () => {
  for (const barrier of ["buttonsAt", "cursorSync"]) {
    const send = c => (barrier === "buttonsAt" ? c.buttonsAt(5, 6, 1) : c.cursorSync());
    const barrierType = barrier === "buttonsAt" ? 9 : 10;

    // Open control lane: the barrier goes out and the trailing flush sends the wheel right after it.
    let { c, wire } = harness();
    c.motion(0, 0, 0, 90); c.absolute(5, 6);
    send(c);
    assert.deepEqual(types(wire), [barrierType, 7], `${barrier}: barrier first, wheel after, no absolute`);
    assert.equal(c.hasPendingMotion(), false);

    // Congested control lane: the barrier is queued, the wheel waits behind it, nothing is dropped.
    let state = "busy";
    ({ c, wire } = harness({ control: () => state }));
    c.motion(0, 0, 0, 90); c.absolute(5, 6);
    send(c);
    assert.equal(c.flushMotion(), false, `${barrier}: motion waits behind the queued barrier`);
    assert.equal(c.hasPendingMotion(), true, `${barrier}: wheel still pending`);
    state = "open"; c.flushControl();
    assert.deepEqual(types(wire), [barrierType, 7], `${barrier}: queued barrier first, wheel after`);
    assert.deepEqual(summarize(wire.filter(w => w.lane === "motion")), { absolutes: [], wheel: { x: 0, y: 90 } }, barrier);
    assertConsecutiveSequences(wire, barrier);
  }
});

test("cursor authority tracks the last pointer packet, not the last wheel", () => {
  const { c } = harness();
  c.absolute(10, 20); c.motion(0, 0, 0, 60); c.flushMotion();
  assert.equal(c.lastCursorSequence, 0); assert.equal(c.lastSentSequence, 1);
  c.motion(0, 0, 0, 30); c.flushMotion();
  assert.equal(c.lastCursorSequence, 0, "wheel-only send leaves cursor authority alone");
  assert.equal(c.lastSentSequence, 2);
  c.buttonsAt(10, 20, 0);
  assert.equal(c.lastCursorSequence, 3);
  c.motion(4, 0); c.flushMotion();
  assert.equal(c.lastCursorSequence, 4, "a relative move takes cursor authority");
});

test("every packet on both lanes takes the next shared sequence across barriers and queued control", () => {
  let state = "open";
  const { c, wire } = harness({ control: () => state });
  c.absolute(1, 1); c.flushMotion();                       // seq 0 (motion)
  state = "busy";
  c.buttonsAt(1, 1, 1); c.motion(0, 0, 0, 30); c.flushMotion(); c.cursorSync();  // 1 and 2 queued; wheel waits
  assert.equal(c.flushMotion(), false, "motion waits behind queued control");
  state = "open"; c.flushControl();                        // seq 1, 2 (control), then wheel seq 3
  c.absolute(2, 2); c.motion(0, 0, 40000, 0); c.flushMotion(); // seq 4 (target), 5 and 6 (chunks)
  c.cursorSync();                                         // seq 7
  assertConsecutiveSequences(wire, "mixed lanes");
  assert.deepEqual(seqs(wire), [0, 1, 2, 3, 4, 5, 6, 7]);
  assert.deepEqual(types(wire), [8, 9, 10, 7, 8, 7, 7, 10]);
  assert.equal(c.seq, 8);
});

test("a reentrant control send cannot reuse or duplicate a packet", () => {
  const wire = []; let nested = false, c;
  c = new InputController({
    scheduleFlush: () => {},
    sendControl: p => {
      wire.push({ lane: "control", bytes: [...p] });
      if (!nested && p[0] === 9) { nested = true; c.flushControl(); c.cursorSync(); }
      return true;
    },
  });
  assert.equal(c.buttonsAt(1, 1, 1), true);
  assert.deepEqual(types(wire), [9, 10], "queued barrier sent once, after the first");
  assertConsecutiveSequences(wire, "reentrant control");
});

test("a reentrant motion flush sends nothing extra and takes no sequence", () => {
  const wire = []; let c;
  c = new InputController({
    scheduleFlush: () => {},
    sendMotion: p => { wire.push({ lane: "motion", bytes: [...p] }); c.flushMotion(); return true; },
  });
  c.absolute(3, 3); c.motion(0, 0, 0, 10);
  assert.equal(c.flushMotion(), true);
  assert.deepEqual(types(wire), [8, 7]);
  assertConsecutiveSequences(wire, "reentrant motion");
});

test("two-finger horizontal drag runs through the real conversion and reaches the wire with Android's sign", () => {
  const gesture = new TouchGesture({ pinchThreshold: 100 });
  gesture.down(1, { x: 0, y: 0 }); gesture.down(2, { x: 100, y: 0 });
  gesture.move(1, { x: 3, y: 0 });
  const [translate] = gesture.move(1, { x: 16, y: 0 });
  assert.equal(translate.type, "translate");
  assert.ok(translate.dx > 0, "fingers moving right give a positive finger delta");

  const wheel = touchTranslateWheel(translate);
  assert.ok(wheel.x < 0, "fingers moving right send a negative wheel X");
  assert.equal(wheel.y, translate.dy, "vertical stays as the finger delta");

  const { c, wire } = harness();
  c.motionNow(0, 0, wheel.x, wheel.y);
  assert.equal(view(wire[0].bytes).getInt16(7, true), Math.round(wheel.x));

  const down = touchTranslateWheel({ dx: 0, dy: 12 });
  assert.equal(down.y, 12, "finger down sends positive wheel Y, as Android does");

  const receiver = readFileSync(new URL("../public/receiver.js", import.meta.url), "utf8");
  assert.match(receiver, /action\.type === "translate"[^\n]*touchTranslateWheel\(action\)/, "receiver converts through the helper");
});

test("client replay fixture: the exact packet bytes match the checked-in capture", () => {
  const fixture = JSON.parse(readFileSync(new URL("./fixtures/scroll-client-replay.json", import.meta.url), "utf8"));
  const packets = [];
  const c = new InputController({
    scheduleFlush: () => {},
    sendMotion: p => { packets.push({ lane: "motion", hex: Buffer.from(p).toString("hex") }); return true; },
    sendControl: p => { packets.push({ lane: "control", hex: Buffer.from(p).toString("hex") }); return true; },
  });
  for (const step of fixture.steps) {
    if (step.op === "flush") c.flushMotion();
    else if (step.op === "absolute") c.absolute(...step.args);
    else if (step.op === "motion") c.motion(...step.args);
    else if (step.op === "buttonsAt") c.buttonsAt(...step.args);
    else assert.fail(`unknown step ${step.op}`);
  }
  assert.deepEqual(packets, fixture.packets);
});
