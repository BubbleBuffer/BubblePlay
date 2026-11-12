import test from "node:test";
import assert from "node:assert/strict";
import { createKeyboardDraft, BACKSPACE_VK, TEXT_CHUNK_BYTES } from "../public/keyboard-draft.js";

function harness(over = {}) {
  const sent = [];
  const state = { ready: true, kind: "default", inputState: "enabled", status: "" };
  const field = { value: "" };
  const timers = [];
  const draft = createKeyboardDraft({
    input: { text: t => { sent.push(["text", t]); return true; }, key: (vk, up) => sent.push(["key", vk, up]) },
    field, inputReady: () => state.ready, desktopKind: () => state.kind,
    desktopInputState: () => state.inputState, setStatus: t => { state.status = t; },
    schedule: cb => { timers.push(cb); return timers.length - 1; }, cancel: () => {}, ...over,
  });
  return { draft, sent, state, field, timers };
}

test("desktop keyboard corrections replay to the typed text", () => {
  const h = harness();
  for (const value of ["helo", "hello", "help"]) { h.field.value = value; h.draft.input({}); }
  let remote = "";
  for (const [kind, a, up] of h.sent) {
    if (kind === "text") remote += a;
    else if (up) remote = remote.slice(0, -1);
  }
  assert.equal(remote, "help");
});

test("long text is split into chunks under the packet limit", () => {
  const h = harness();
  h.field.value = "x".repeat(TEXT_CHUNK_BYTES + 10); h.draft.input({});
  assert.deepEqual(h.sent.map(([, t]) => t.length), [TEXT_CHUNK_BYTES, 10]);
});

test("composition input is held until the composition ends", () => {
  const h = harness();
  h.draft.compositionStart();
  h.field.value = "ni"; h.draft.input({});
  assert.deepEqual(h.sent, []);
  h.field.value = "你"; h.draft.compositionEnd({});
  assert.deepEqual(h.sent, [["text", "你"]]);
});

test("secure desktop sends protected input once and clears the draft; paste is refused", () => {
  const h = harness();
  h.state.kind = "secure";
  h.field.value = "a"; h.draft.input({ data: "a", inputType: "insertText" });
  assert.deepEqual(h.sent, [["text", "a"]]);
  assert.equal(h.field.value, "");
  assert.equal(h.state.status, "Protected input sent.");
  h.draft.input({ data: "abc", inputType: "insertFromPaste" });
  assert.deepEqual(h.sent, [["text", "a"]]);
  assert.match(h.state.status, /disabled on secure desktop/);
});

test("secure composition commits once: the trailing input consumes it, the timer then does nothing", () => {
  const h = harness();
  h.state.kind = "secure";
  h.draft.compositionStart();
  h.draft.compositionEnd({ data: "é" });
  h.draft.input({ data: "é", inputType: "insertText" });
  h.timers.forEach(cb => cb());
  assert.deepEqual(h.sent, [["text", "é"]]);
});
